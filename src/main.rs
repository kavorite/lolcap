use std::{
    fs,
    sync::{Arc, atomic::{AtomicBool, AtomicUsize, AtomicPtr, Ordering}},
    thread::{self, JoinHandle},
    time::Instant,
    fs::File,
    io::Write,
};
use arrow::datatypes::{Schema, TimeUnit};
use windows::{
    Win32::UI::WindowsAndMessaging::{
        WNDCLASSW, CreateWindowExW, DefWindowProcW, GetMessageW,
        PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage,
        DispatchMessageW, WINDOW_EX_STYLE, WINDOW_STYLE, CW_USEDEFAULT,
        WM_DESTROY, WM_USER, MSG, GetWindowTextLengthW, GetWindowTextW,
        SetWindowsHookExW, UnhookWindowsHookEx, CallNextHookEx,
        WH_KEYBOARD_LL, WH_MOUSE_LL, KBDLLHOOKSTRUCT, MSLLHOOKSTRUCT, HHOOK,
        GetForegroundWindow, PeekMessageW, PEEK_MESSAGE_REMOVE_TYPE, WM_QUIT,
    },
    Win32::Foundation::{HWND, WPARAM, LPARAM, LRESULT},
    Win32::UI::Accessibility::*,
    Win32::System::LibraryLoader::GetModuleHandleW,
    core::*,
};
use std::sync::Mutex;
use once_cell::sync::Lazy;
use anyhow::{Result, Context, bail, ensure};
use arrow::{
    array::{Int32Array, UInt32Array, TimestampMicrosecondArray},
    record_batch::RecordBatch,
};
use parquet::{
    arrow::ArrowWriter,
    basic::Compression,
    file::properties::WriterProperties,
};
use windows_capture::{
    capture::{Context as CaptureContext, GraphicsCaptureApiHandler},
    encoder::{VideoSettingsBuilder, AudioSettingsBuilder, ContainerSettingsBuilder, VideoEncoder},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    settings::{ColorFormat, CursorCaptureSettings, DrawBorderSettings, Settings},
    window::Window,
};

// Constants that were in wrong modules
const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;
const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;
const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;

// Change to store just the necessary data in the global state
#[derive(Clone)]
struct RecorderState {
    window_title: String,
    is_recording: Arc<AtomicBool>,
    output_directory: String,
    session_id: Arc<AtomicUsize>,
    last_window_change: Arc<Mutex<Instant>>,
    capture_ready: Arc<AtomicBool>,
}

static RECORDER: Lazy<Arc<Mutex<Option<RecorderState>>>> = Lazy::new(|| Arc::new(Mutex::new(None)));

struct InputBuffer {
    timestamps: Vec<i64>,
    event_types: Vec<u32>,
    codes: Vec<u32>,
    x_coords: Vec<i32>,
    y_coords: Vec<i32>,
    flags: Vec<u32>,
}

const BUFFER_SIZE: usize = 1024;
static INPUT_WRITER: Lazy<Mutex<Option<ArrowWriter<File>>>> = Lazy::new(|| Mutex::new(None));
static START_TIME: Lazy<Mutex<Option<Instant>>> = Lazy::new(|| Mutex::new(None));
static INPUT_BUFFER: Lazy<Mutex<InputBuffer>> = Lazy::new(|| Mutex::new(InputBuffer {
    timestamps: Vec::with_capacity(BUFFER_SIZE),
    event_types: Vec::with_capacity(BUFFER_SIZE),
    codes: Vec::with_capacity(BUFFER_SIZE),
    x_coords: Vec::with_capacity(BUFFER_SIZE),
    y_coords: Vec::with_capacity(BUFFER_SIZE),
    flags: Vec::with_capacity(BUFFER_SIZE),
}));

static INPUT_SCHEMA: Lazy<Arc<Schema>> = Lazy::new(|| Arc::new(Schema::new(vec![
            arrow::datatypes::Field::new("timestamp_us", arrow::datatypes::DataType::Timestamp(TimeUnit::Microsecond, None), false),
            arrow::datatypes::Field::new("event_type", arrow::datatypes::DataType::UInt32, false),
            arrow::datatypes::Field::new("code", arrow::datatypes::DataType::UInt32, false),
            arrow::datatypes::Field::new("x", arrow::datatypes::DataType::Int32, false),
            arrow::datatypes::Field::new("y", arrow::datatypes::DataType::Int32, false),
            arrow::datatypes::Field::new("flags", arrow::datatypes::DataType::UInt32, false),
        ])));

struct GameRecorder {
    state: RecorderState,
    hook: Option<HWINEVENTHOOK>,
    recording_threads: Vec<JoinHandle<()>>,
    keyboard_hook: Option<HHOOK>,
    mouse_hook: Option<HHOOK>,
    error_sender: std::sync::mpsc::Sender<anyhow::Error>,
    error_receiver: std::sync::mpsc::Receiver<anyhow::Error>,
    window: HWND,
}

// New capture handler struct
struct CaptureHandler {
    encoder: Option<VideoEncoder>,
    state: RecorderState,
    start_time: Instant,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = RecorderState;
    type Error = anyhow::Error;

    fn new(ctx: CaptureContext<Self::Flags>) -> Result<Self, Self::Error> {
        let window = Window::from_name(&ctx.flags.window_title)?;
        let rect = window.rect()?;
        let source_width = (rect.right - rect.left) as u32;
        let source_height = (rect.bottom - rect.top) as u32;
        println!("Source window size: {}x{}", source_width, source_height);

        let encoder = VideoEncoder::new(
            VideoSettingsBuilder::new(source_width, source_height)
                .frame_rate(24)
                .bitrate(16384),
            AudioSettingsBuilder::default().disabled(true),
            ContainerSettingsBuilder::default(),
            &ctx.flags.get_output_path(),
        )?;

        println!("Encoder initialized");
        Ok(Self {
            encoder: Some(encoder),
            state: ctx.flags,
            start_time: Instant::now(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if !self.state.is_recording.load(Ordering::SeqCst) {
            if let Some(encoder) = self.encoder.take() {
                // Only finish the encoder if we've processed at least one frame
                println!("Finishing encoder");
                match encoder.finish() {
                    Ok(_) => println!("Encoder finished successfully"),
                    Err(e) => println!("Error finishing encoder: {}", e),
                }
            }
            capture_control.stop();
            return Ok(());
        }

        // Add frame processing status
        print!("\rRecording for: {} seconds", self.start_time.elapsed().as_secs());
        std::io::stdout().flush()?;

        if let Some(encoder) = &mut self.encoder {
            encoder.send_frame(frame).context("Failed to send frame to encoder")?;
        }
        
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("\nCapture session ended");
        if let Some(encoder) = self.encoder.take() {
            match encoder.finish() {
                Ok(_) => println!("Encoder finished successfully on close"),
                Err(e) => println!("Error finishing encoder on close: {}", e),
            }
        }
        Ok(())
    }
}

impl GameRecorder {
    fn new(window_title: &str, output_dir: &str) -> Result<Self> {
        fs::create_dir_all(output_dir)?;

        let (error_sender, error_receiver) = std::sync::mpsc::channel();

        let starting_session = fs::read_dir(output_dir)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                entry.file_name()
                    .to_str()
                    .and_then(|name| name.split('_').next())
                    .and_then(|id| id.parse::<usize>().ok())
            })
            .max()
            .map_or(0, |max| max + 1);

        Ok(Self {
            state: RecorderState {
                window_title: window_title.to_string(),
                is_recording: Arc::new(AtomicBool::new(false)),
                output_directory: output_dir.to_string(),
                session_id: Arc::new(AtomicUsize::new(starting_session)),
                last_window_change: Arc::new(Mutex::new(Instant::now())),
                capture_ready: Arc::new(AtomicBool::new(false)),
            },
            hook: None,
            recording_threads: Vec::new(),
            keyboard_hook: None,
            mouse_hook: None,
            error_sender,
            error_receiver,
            window: HWND(std::ptr::null_mut()),
        })
    }

    fn start_recording(&mut self) -> Result<()> {
        // Clean up any old recording threads that have finished
        self.recording_threads.retain(|handle| !handle.is_finished());
        
        self.stop_recording()?;

        let input_path = self.state.get_output_path().replace(".mp4", "_inputs.parquet");
        println!("Creating input file: {}", input_path);
        let input_file = File::create(input_path)?;

        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .build();

        *INPUT_WRITER.lock().unwrap() = Some(ArrowWriter::try_new(
            input_file,
            Arc::clone(&INPUT_SCHEMA),
            Some(props),
        )?);
        *START_TIME.lock().unwrap() = Some(Instant::now());

        // Set recording flag before starting capture
        self.state.is_recording.store(true, Ordering::SeqCst);

        // Set up input hooks
        unsafe {
            println!("Setting up input hooks");
            self.keyboard_hook = Some(SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(Self::keyboard_hook_proc),
                None,
                0,
            )?);

            self.mouse_hook = Some(SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(Self::mouse_hook_proc),
                None,
                0,
            )?);
        }

        // Start video capture using windows-capture
        let video_state = self.state.clone();
        let error_sender = self.error_sender.clone();
        let video_thread = thread::spawn(move || {
            println!("Video capture thread started");
            
            // First verify we're actually focused on the target window
            if let Some(current_title) = unsafe { get_window_title(GetForegroundWindow()) } {
                if current_title != video_state.window_title {
                    println!("Wrong window focused (current: {}, target: {})", 
                        current_title, video_state.window_title);
                    return;
                }
            }
            
            // Try to get a valid window size with retries
            let mut retry_count = 0;
            let window = loop {
                match Window::from_name(&video_state.window_title) {
                    Ok(w) => {
                        // Verify window size is valid
                        if let Ok(rect) = w.rect() {
                            let width = (rect.right - rect.left) as u32;
                            let height = (rect.bottom - rect.top) as u32;
                            if width > 1 && height > 1 {
                                println!("Found valid window size: {}x{}", width, height);
                                break w;
                            }
                        }
                    },
                    Err(e) => println!("Failed to get window: {}", e),
                }
                
                retry_count += 1;
                if retry_count > 10 {
                    if let Err(_) = error_sender.send(anyhow::Error::msg("Failed to get valid window size after 10 retries")) {
                        return;
                    }
                    return;
                }
                thread::sleep(std::time::Duration::from_millis(100));
            };

            let settings = Settings::new(
                window,
                CursorCaptureSettings::WithCursor,
                DrawBorderSettings::WithoutBorder,
                ColorFormat::Bgra8,
                video_state.clone(),
            );
            
            println!("Starting capture session");
            if let Err(e) = CaptureHandler::start(settings) {
                println!("Failed to start capture: {}", e);
                let _ = error_sender.send(anyhow::Error::new(e));
            }
        });

        self.recording_threads.push(video_thread);
        println!("=== RECORDING STARTED ===");
        Ok(())
    }

    fn stop_recording(&mut self) -> Result<()> {
        // Set recording flag to false first
        self.state.is_recording.store(false, Ordering::SeqCst);
        
        // Clean up hooks
        if let Some(hook) = self.keyboard_hook.take() {
            unsafe { UnhookWindowsHookEx(hook)? };
        }
        if let Some(hook) = self.mouse_hook.take() {
            unsafe { UnhookWindowsHookEx(hook)? };
        }

        // Write any remaining buffered events and close writer
        {
            let buffer = INPUT_BUFFER.lock().unwrap();
            if !buffer.timestamps.is_empty() {
                if let Some(writer) = &mut *INPUT_WRITER.lock().unwrap() {
                    let batch = RecordBatch::try_new(
                        Arc::clone(&INPUT_SCHEMA),
                        vec![
                            Arc::new(TimestampMicrosecondArray::from(buffer.timestamps.clone())),
                            Arc::new(UInt32Array::from(buffer.event_types.clone())),
                            Arc::new(UInt32Array::from(buffer.codes.clone())),
                            Arc::new(Int32Array::from(buffer.x_coords.clone())),
                            Arc::new(Int32Array::from(buffer.y_coords.clone())),
                            Arc::new(UInt32Array::from(buffer.flags.clone())),
                        ],
                    )?;
                    writer.write(&batch)?;
                    writer.flush()?;
                }
            }
        }

        if let Some(writer) = INPUT_WRITER.lock().unwrap().take() {
            writer.close()?;
        }
        *START_TIME.lock().unwrap() = None;

        // Clear the input buffer
        let mut buffer = INPUT_BUFFER.lock().unwrap();
        buffer.timestamps.clear();
        buffer.event_types.clear();
        buffer.codes.clear();
        buffer.x_coords.clear();
        buffer.y_coords.clear();
        buffer.flags.clear();

        // Don't wait for the recording threads here
        // Just let them finish on their own when they detect recording has stopped
        println!("Recording stopped successfully");
        Ok(())
    }

    fn check_thread_errors(&mut self) -> Result<()> {
        // Check for any errors sent through the channel
        if let Ok(error) = self.error_receiver.try_recv() {
            bail!("Recording thread error: {}", error);
        }

        // Check if any threads have finished
        let mut i = 0;
        while i < self.recording_threads.len() {
            if self.recording_threads[i].is_finished() {
                let handle = self.recording_threads.remove(i);
                handle.join()
                    .map_err(|e| anyhow::anyhow!("Recording thread panicked: {:?}", e))?;
            } else {
                i += 1;
            }
        }

        // Ensure we still have active threads if recording
        if self.state.is_recording.load(Ordering::SeqCst) {
            ensure!(!self.recording_threads.is_empty(), "All recording threads terminated unexpectedly");
        }

        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        unsafe {
            *RECORDER.lock().unwrap() = Some(self.state.clone());

            let window_class = w!("GameRecorderClass");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(Self::window_proc),
                lpszClassName: window_class,
                hInstance: GetModuleHandleW(None)?.into(),
                ..Default::default()
            };

            RegisterClassW(&wc);

            let window = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                window_class,
                w!("GameRecorder"),
                WINDOW_STYLE::default(),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                None,
                None,
            ).context("Failed to create window")?;

            self.window = window;

            RECORDER_WINDOW.store(window.0, Ordering::SeqCst);

            let hook = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(Self::win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );

            self.hook = Some(hook);

            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                match message.message {
                    WM_USER => {
                        match message.wParam.0 {
                            1 => self.start_recording()?,
                            0 => self.stop_recording()?,
                            _ => {}
                        }
                    },
                    _ => {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                self.check_thread_errors()?;
            }
        }
        Ok(())
    }

    unsafe extern "system" fn win_event_proc(
        _hook: HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        _id_object: i32,
        _id_child: i32,
        _id_event_thread: u32,
        _dwms_event_time: u32,
    ) {
        if event != EVENT_SYSTEM_FOREGROUND {
            return;
        }

        if let Some(title) = get_window_title(hwnd) {
            println!("Window focus changed to: {}", title);

            if title == "Task Switching" {
                println!("Ignoring task switcher");
                return;
            }

            if let Some(state) = RECORDER.lock().unwrap().as_ref() {
                let title_matches = title == state.window_title;
                let recorder_window = HWND(RECORDER_WINDOW.load(Ordering::SeqCst));
                
                // Only use debounce for rapid switches between different windows
                let should_debounce = {
                    let last_change = state.last_window_change.lock().unwrap();
                    last_change.elapsed() < std::time::Duration::from_millis(500)
                };

                // Update last window change time
                if let Ok(mut last_change) = state.last_window_change.lock() {
                    *last_change = Instant::now();
                }
                
                if title_matches {
                    if !should_debounce {
                        println!("Target window focused - sending start recording message");
                        state.session_id.fetch_add(1, Ordering::SeqCst);
                        let _ = PostMessageW(recorder_window, WM_USER, WPARAM(1), LPARAM(0));
                    } else {
                        println!("Ignoring rapid window switch to target window");
                    }
                } else {
                    println!("Other window focused - sending stop recording message");
                    let _ = PostMessageW(recorder_window, WM_USER, WPARAM(0), LPARAM(0));
                }
            }
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe extern "system" fn keyboard_hook_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code >= 0 {
            if let Some(start_time) = &*START_TIME.lock().unwrap() {
                let kbd_struct = *(lparam.0 as *const KBDLLHOOKSTRUCT);
                let timestamp = start_time.elapsed().as_micros() as i64;
                
                let mut buffer = INPUT_BUFFER.lock().unwrap();
                buffer.timestamps.push(timestamp);
                buffer.event_types.push(0);  // keyboard
                buffer.codes.push(kbd_struct.vkCode);
                buffer.x_coords.push(0);
                buffer.y_coords.push(0);
                buffer.flags.push(kbd_struct.flags.0);

                if buffer.timestamps.len() >= BUFFER_SIZE {
                    if let Some(writer) = &mut *INPUT_WRITER.lock().unwrap() {
                        let batch = RecordBatch::try_new(
                            Arc::clone(&INPUT_SCHEMA),
                            vec![
                                Arc::new(TimestampMicrosecondArray::from(buffer.timestamps.clone())),
                                Arc::new(UInt32Array::from(buffer.event_types.clone())),
                                Arc::new(UInt32Array::from(buffer.codes.clone())),
                                Arc::new(Int32Array::from(buffer.x_coords.clone())),
                                Arc::new(Int32Array::from(buffer.y_coords.clone())),
                                Arc::new(UInt32Array::from(buffer.flags.clone())),
                            ],
                        ).unwrap();
                        let _ = writer.write(&batch);
                        
                        buffer.timestamps.clear();
                        buffer.event_types.clear();
                        buffer.codes.clear();
                        buffer.x_coords.clear();
                        buffer.y_coords.clear();
                        buffer.flags.clear();
                    }
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    unsafe extern "system" fn mouse_hook_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code >= 0 {
            if let Some(start_time) = &*START_TIME.lock().unwrap() {
                let mouse_struct = *(lparam.0 as *const MSLLHOOKSTRUCT);
                let timestamp = start_time.elapsed().as_micros() as i64;
                
                let mut buffer = INPUT_BUFFER.lock().unwrap();
                buffer.timestamps.push(timestamp);
                buffer.event_types.push(1);  // mouse
                buffer.codes.push(mouse_struct.mouseData);
                buffer.x_coords.push(mouse_struct.pt.x);
                buffer.y_coords.push(mouse_struct.pt.y);
                buffer.flags.push(mouse_struct.flags);

                if buffer.timestamps.len() >= 1000 {
                    if let Some(writer) = &mut *INPUT_WRITER.lock().unwrap() {
                        let batch = RecordBatch::try_new(
                            Arc::clone(&INPUT_SCHEMA),
                            vec![
                                Arc::new(TimestampMicrosecondArray::from(buffer.timestamps.clone())),
                                Arc::new(UInt32Array::from(buffer.event_types.clone())),
                                Arc::new(UInt32Array::from(buffer.codes.clone())),
                                Arc::new(Int32Array::from(buffer.x_coords.clone())),
                                Arc::new(Int32Array::from(buffer.y_coords.clone())),
                                Arc::new(UInt32Array::from(buffer.flags.clone())),
                            ],
                        ).unwrap();
                        let _ = writer.write(&batch);
                        
                        buffer.timestamps.clear();
                        buffer.event_types.clear();
                        buffer.codes.clear();
                        buffer.x_coords.clear();
                        buffer.y_coords.clear();
                        buffer.flags.clear();
                    }
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

impl RecorderState {
    fn get_output_path(&self) -> String {
        format!("{}/{}.mp4", 
            self.output_directory,
            self.session_id.load(Ordering::SeqCst)
        )
    }
}

unsafe fn get_window_title(hwnd: HWND) -> Option<String> {
    let len = GetWindowTextLengthW(hwnd);
    if len == 0 { return None; }

    let mut title = vec![0u16; len as usize + 1];
    let len = GetWindowTextW(hwnd, &mut title);
    if len == 0 { return None; }

    String::from_utf16_lossy(&title[..len as usize]).into()
}

// Add a static for the window handle
static RECORDER_WINDOW: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

fn main() -> Result<()> {
    let mut recorder = GameRecorder::new(
        "League of Legends (TM) Client",
        "./recordings"
    )?;
    recorder.run()
}
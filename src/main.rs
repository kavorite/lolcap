use std::{
    fs,
    sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}},
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, Instant},
    path::Path,
    ptr::addr_of,
    io::Write,
    mem,
    fs::File,
    collections::VecDeque,
};
use rav1e::prelude::*;
use arrow::datatypes::{Schema, TimeUnit};
use windows::{
    Win32::UI::WindowsAndMessaging::*,
    Win32::Foundation::*,
    Win32::UI::Input::KeyboardAndMouse::*,
    Win32::UI::Accessibility::*,
    Win32::System::LibraryLoader::GetModuleHandleW,
    Win32::Graphics::Gdi::{GetDC, ReleaseDC, BitBlt, CreateCompatibleDC, CreateCompatibleBitmap, 
                          SelectObject, SRCCOPY, DeleteObject, GetDIBits, BITMAPINFO, 
                          BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteDC, BI_RGB},
    core::*,
    Win32::Graphics::Direct3D11::*,
    Win32::Graphics::Dxgi::*,
    Win32::Graphics::Direct3D::*,
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
    recording_threads: Vec<JoinHandle<Result<()>>>,
    keyboard_hook: Option<HHOOK>,
    mouse_hook: Option<HHOOK>,
}

// New capture handler struct
struct CaptureHandler {
    encoder: Option<VideoEncoder>,
    state: RecorderState,
    target_width: u32,
    target_height: u32,
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

        // Target dimensions for the final video
        let target_width = 854;
        let target_height = 480;

        // Create encoder at source resolution
        let encoder = VideoEncoder::new(
            VideoSettingsBuilder::new(source_width, source_height)
                .frame_rate(30)
                .bitrate(5000),
            AudioSettingsBuilder::default().disabled(true),
            ContainerSettingsBuilder::default(),
            &ctx.flags.get_output_path(),
        )?;

        Ok(Self {
            encoder: Some(encoder),
            state: ctx.flags,
            target_width,
            target_height,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if !self.state.is_recording.load(Ordering::SeqCst) {
            if let Some(encoder) = self.encoder.take() {
                encoder.finish()?;
            }
            capture_control.stop();
            return Ok(());
        }

        // Now we're capturing at full window resolution
        if let Some(encoder) = &mut self.encoder {
            encoder.send_frame(frame)?;
        }

        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("Capture session ended");
        Ok(())
    }
}

impl GameRecorder {
    fn new(window_title: &str, output_dir: &str) -> Result<Self> {
        fs::create_dir_all(output_dir)?;

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
            },
            hook: None,
            recording_threads: Vec::new(),
            keyboard_hook: None,
            mouse_hook: None,
        })
    }

    fn start_recording(&mut self) -> Result<()> {
        println!("=== START RECORDING ===");
        self.stop_recording();

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
        let video_thread = thread::spawn(move || -> Result<()> {
            let window = Window::from_name(&video_state.window_title)?;

            let settings = Settings::new(
                window,
                CursorCaptureSettings::WithCursor,
                DrawBorderSettings::WithoutBorder,
                ColorFormat::Bgra8,
                video_state,
            );

            CaptureHandler::start(settings)?;
            Ok(())
        });

        self.recording_threads.push(video_thread);
        println!("=== RECORDING STARTED ===");
        Ok(())
    }

    fn stop_recording(&mut self) {
        println!("In stop_recording"); // Debug logging
        
        // Clean up hooks
        if let Some(hook) = self.keyboard_hook.take() {
            unsafe { UnhookWindowsHookEx(hook); }
        }
        if let Some(hook) = self.mouse_hook.take() {
            unsafe { UnhookWindowsHookEx(hook); }
        }

        // Write any remaining buffered events and ensure they're flushed to disk
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
                    ).unwrap();
                    let _ = writer.write(&batch);
                    // Force flush the writer
                    let _ = writer.flush();
                }
            }
        }

        // Close Arrow writer
        if let Some(writer) = INPUT_WRITER.lock().unwrap().take() {
            if let Err(e) = writer.close() {
                eprintln!("Error closing input writer: {}", e);
            }
        }
        *START_TIME.lock().unwrap() = None;

        // Join video thread and ensure it's properly flushed
        while let Some(handle) = self.recording_threads.pop() {
            if let Err(e) = handle.join().unwrap() {
                eprintln!("Recording thread error: {}", e);
            }
        }

        // Clear the input buffer
        let mut buffer = INPUT_BUFFER.lock().unwrap();
        buffer.timestamps.clear();
        buffer.event_types.clear();
        buffer.codes.clear();
        buffer.x_coords.clear();
        buffer.y_coords.clear();
        buffer.flags.clear();
    }

    fn check_thread_errors(&mut self) {
        let mut i = 0;
        while i < self.recording_threads.len() {
            if self.recording_threads[i].is_finished() {
                println!("Thread finished, checking for errors"); // Debug logging
                match self.recording_threads.remove(i).join().unwrap() {
                    Ok(_) => println!("Thread completed successfully"),
                    Err(e) => {
                        eprintln!("Thread error: {}", e);
                        self.state.is_recording.store(false, Ordering::SeqCst);
                    }
                }
            } else {
                i += 1;
            }
        }
    }

    fn run(&mut self) -> Result<()> {
        unsafe {
            println!("Starting recorder run");
            *RECORDER.lock().unwrap() = Some(self.state.clone());

            let window_class = w!("GameRecorderClass");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(Self::window_proc),
                lpszClassName: window_class,
                hInstance: GetModuleHandleW(None)?.into(),
                ..Default::default()
            };

            RegisterClassW(&wc);

            let hwnd = CreateWindowExW(
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
            )?;

            println!("Created window: {:?}", hwnd);

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
            println!("Set up event hook: {:?}", hook);

            let mut message = MSG::default();
            println!("Entering message loop");
            
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                match message.message {
                    WM_USER => {
                        match message.wParam.0 {
                            1 => {
                                println!("=== Starting recording ===");
                                if let Err(e) = self.start_recording() {
                                    println!("Failed to start recording: {:#}", e);
                                    self.state.is_recording.store(false, Ordering::SeqCst);
                                }
                            },
                            0 => {
                                println!("=== Stopping recording ===");
                                self.stop_recording();
                            },
                            _ => {}
                        }
                    },
                    _ => {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }

                self.check_thread_errors();
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
        // Only process foreground window changes
        if event != EVENT_SYSTEM_FOREGROUND {
            return;
        }

        // Get the title of the window that's becoming foreground
        if let Some(title) = get_window_title(hwnd) {
            // Ignore Task Switching window
            if title == "Task Switching" {
                return;
            }

            if let Some(state) = RECORDER.lock().unwrap().as_ref() {
                println!("Window focus changed to: {}", title);
                
                let title_matches = title == state.window_title;
                let is_recording = state.is_recording.load(Ordering::SeqCst);

                println!("State check - title_matches: {}, is_recording: {}", title_matches, is_recording);

                match (title_matches, is_recording) {
                    (true, false) => {
                        println!("Game window focused, starting recording");
                        state.session_id.fetch_add(1, Ordering::SeqCst);
                        state.is_recording.store(true, Ordering::SeqCst);
                        PostMessageW(Some(HWND(std::ptr::null_mut())), WM_USER, WPARAM(1), LPARAM(0));
                    },
                    (false, true) => {
                        println!("Game window unfocused, stopping recording");
                        state.is_recording.store(false, Ordering::SeqCst);
                        PostMessageW(Some(HWND(std::ptr::null_mut())), WM_USER, WPARAM(0), LPARAM(0));
                    },
                    _ => {}
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

fn main() -> Result<()> {
    let mut recorder = GameRecorder::new(
        "League of Legends (TM) Client",
        "./recordings"
    )?;
    recorder.run()
}
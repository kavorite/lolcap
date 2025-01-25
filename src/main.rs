use std::{
    fs,
    sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}},
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, Instant},
    path::Path,
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
                          BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteDC},
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
            },
            hook: None,
            recording_threads: Vec::new(),
            keyboard_hook: None,
            mouse_hook: None,
        })
    }

    fn start_recording(&mut self) -> Result<()> {
        self.stop_recording();

        let input_path = self.state.get_output_path().replace(".ivf", "_inputs.parquet");
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

        unsafe {
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

        let video_active = Arc::clone(&self.state.is_recording);
        let video_state = self.state.clone();
        let video_thread = thread::spawn(move || -> Result<()> {
            let output_path = video_state.get_output_path();
            let mut output_file = File::create(&output_path)?;
            
            let enc = EncoderConfig {
                width: 1920,
                height: 1080,
                speed_settings: SpeedSettings::from_preset(9),
                time_base: Rational::new(1, 60),
                ..Default::default()
            };
            let cfg = Config::new().with_encoder_config(enc.clone());
            let mut ctx: rav1e::Context<u8> = cfg.new_context()
                .map_err(|e| anyhow::anyhow!("Failed to create encoder context: {}", e))?;
            
            unsafe {
                let screen_dc = GetDC(None);
                let mem_dc = CreateCompatibleDC(Some(screen_dc));
                ensure!(!mem_dc.is_invalid(), "Failed to create compatible DC");

                let bitmap = CreateCompatibleBitmap(screen_dc, enc.width as i32, enc.height as i32);
                ensure!(!bitmap.is_invalid(), "Failed to create bitmap");

                let old_obj = SelectObject(mem_dc, bitmap.into());
                ensure!(!old_obj.is_invalid(), "Failed to select bitmap");
                
                let mut bi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: enc.width as i32,
                        biHeight: -(enc.height as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: 0,
                        biSizeImage: 0,
                        biXPelsPerMeter: 0,
                        biYPelsPerMeter: 0,
                        biClrUsed: 0,
                        biClrImportant: 0,
                    },
                    bmiColors: [Default::default(); 1],
                };
                
                while video_active.load(Ordering::SeqCst) {
                    let res = BitBlt(mem_dc, 0, 0, enc.width as i32, enc.height as i32,
                                   Some(screen_dc), 0, 0, SRCCOPY);
                    res.context("Failed to capture screen")?;
                    
                    let mut pixels = vec![0u8; (enc.width * enc.height * 4) as usize];
                    let scan_lines = GetDIBits(mem_dc, bitmap, 0, enc.height as u32,
                                             Some(pixels.as_mut_ptr() as *mut _),
                                             &mut bi, DIB_RGB_COLORS);
                    ensure!(scan_lines != 0, "Failed to get bitmap data");
                    
                    let mut rgb = Vec::with_capacity((enc.width * enc.height * 3) as usize);
                    for chunk in pixels.chunks_exact(4) {
                        rgb.push(chunk[2]);
                        rgb.push(chunk[1]);
                        rgb.push(chunk[0]);
                    }
                    
                    let mut frame = ctx.new_frame();
                    for p in &mut frame.planes {
                        let stride = (enc.width + p.cfg.xdec) >> p.cfg.xdec;
                        p.copy_from_raw_u8(&rgb, stride, 1);
                    }
                    
                    match ctx.send_frame(frame) {
                        Ok(_) => {}
                        Err(EncoderStatus::EnoughData) => {
                            while let Ok(packet) = ctx.receive_packet() {
                                output_file.write_all(&packet.data)?;
                            }
                        }
                        Err(e) => bail!("Failed to send frame: {}", e),
                    }
                    
                    thread::sleep(Duration::from_millis(16));
                }
                
                let res = SelectObject(mem_dc, old_obj);
                ensure!(!res.is_invalid(), "Failed to restore old object");
                
                let res = DeleteObject(bitmap.into());
                ensure!(res.0 != 0, "Failed to delete bitmap");
                
                let res = DeleteDC(mem_dc);
                ensure!(res.0 != 0, "Failed to delete DC");
                
                let res = ReleaseDC(None, screen_dc);
                ensure!(res == 1, "Failed to release DC");
                
                ctx.flush();
                while let Ok(packet) = ctx.receive_packet() {
                    output_file.write_all(&packet.data)?;
                }
            }
            Ok(())
        });

        self.recording_threads.push(video_thread);
        Ok(())
    }

    fn stop_recording(&mut self) {
        // Clean up hooks
        if let Some(hook) = self.keyboard_hook.take() {
            unsafe { UnhookWindowsHookEx(hook); }
        }
        if let Some(hook) = self.mouse_hook.take() {
            unsafe { UnhookWindowsHookEx(hook); }
        }

        // Write any remaining buffered events
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
            }
        }

        // Close Arrow writer
        if let Some(writer) = INPUT_WRITER.lock().unwrap().take() {
            if let Err(e) = writer.close() {
                eprintln!("Error closing input writer: {}", e);
            }
        }
        *START_TIME.lock().unwrap() = None;

        // Join video thread
        while let Some(handle) = self.recording_threads.pop() {
            if let Err(e) = handle.join().unwrap() {
                eprintln!("Recording thread error: {}", e);
            }
        }
    }

    fn check_thread_errors(&mut self) {
        let mut i = 0;
        while i < self.recording_threads.len() {
            if self.recording_threads[i].is_finished() {
                // Remove and take ownership of the handle
                if let Err(e) = self.recording_threads.remove(i).join().unwrap() {
                    eprintln!("Thread error: {}", e);
                    self.state.is_recording.store(false, Ordering::SeqCst);
                }
            } else {
                i += 1;
            }
        }
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

            let _hwnd = CreateWindowExW(
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
            );

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
                // Check for thread errors periodically
                self.check_thread_errors();

                // Start recording if flag is set but threads aren't running
                if self.state.is_recording.load(Ordering::SeqCst) 
                    && self.recording_threads.is_empty() {
                    self.start_recording()?;
                }

                // Stop recording if flag is cleared but threads are still running
                if !self.state.is_recording.load(Ordering::SeqCst) 
                    && !self.recording_threads.is_empty() {
                    self.stop_recording();
                }

                let _ = TranslateMessage(&message);
                let _ = DispatchMessageW(&message);
            }

            // Cleanup
            if let Some(hook) = self.hook {
                let _ = UnhookWinEvent(hook);
            }
            self.stop_recording();
            *RECORDER.lock().unwrap() = None;
        }
        Ok(())
    }

    unsafe extern "system" fn win_event_proc(
        _hook: HWINEVENTHOOK,
        _event: u32,
        hwnd: HWND,
        _id_object: i32,
        _id_child: i32,
        _id_event_thread: u32,
        _dwms_event_time: u32,
    ) {
        if let Some(title) = get_window_title(hwnd) {
            if let Some(state) = RECORDER.lock().unwrap().as_ref() {
                let title_matches = title == state.window_title;
                let is_recording = state.is_recording.load(Ordering::SeqCst);

                match (title_matches, is_recording) {
                    (true, false) => {
                        state.session_id.fetch_add(1, Ordering::SeqCst);
                        state.is_recording.store(true, Ordering::SeqCst);
                        // Thread management happens in the main loop
                    },
                    (false, true) => {
                        state.is_recording.store(false, Ordering::SeqCst);
                        // Thread cleanup happens in the main loop
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
        format!("{}/{}.ivf", 
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
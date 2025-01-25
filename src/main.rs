use std::{
    fs,
    sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}},
    thread,
    time::{Duration, SystemTime},
    path::Path,
    io::Write,
};
use rav1e::prelude::*;
use windows::{
    Win32::UI::WindowsAndMessaging::*,
    Win32::Foundation::*,
    Win32::UI::Input::KeyboardAndMouse::*,
    Win32::UI::Accessibility::*,
    Win32::System::LibraryLoader::GetModuleHandleW,
    Win32::Graphics::Gdi::{GetDC, ReleaseDC},
    core::*,
};
use std::sync::Mutex;
use once_cell::sync::Lazy;
use anyhow::{Result, Context as _};

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
    session_id: usize,
    starting_chunk: usize,
}

static RECORDER: Lazy<Arc<Mutex<Option<RecorderState>>>> = Lazy::new(|| Arc::new(Mutex::new(None)));

struct GameRecorder {
    state: RecorderState,
    hook: Option<HWINEVENTHOOK>,
}

impl GameRecorder {
    fn new(window_title: &str, output_dir: &str) -> Result<Self> {
        // Create output directory if it doesn't exist
        fs::create_dir_all(output_dir)?;

        // Find highest existing session ID
        let session_id = fs::read_dir(output_dir)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                entry.file_name()
                    .to_str()
                    .and_then(|name| name.split('_').next())
                    .and_then(|id| id.parse::<usize>().ok())
            })
            .max()
            .map_or(0, |max| max + 1);

        // Find highest chunk number for this session
        let starting_chunk = fs::read_dir(output_dir)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with(&session_id.to_string()) {
                    return None;
                }
                name.split('_')
                    .nth(1)
                    .and_then(|chunk| chunk.parse::<usize>().ok())
            })
            .max()
            .map_or(0, |max| max + 1);

        Ok(Self {
            state: RecorderState {
                window_title: window_title.to_string(),
                is_recording: Arc::new(AtomicBool::new(false)),
                output_directory: output_dir.to_string(),
                session_id,
                starting_chunk,
            },
            hook: None,
        })
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
                let _ = TranslateMessage(&message);
                let _ = DispatchMessageW(&message);
            }

            if let Some(hook) = self.hook {
                let _ = UnhookWinEvent(hook);
            }
            
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
                    (true, false) => state.is_recording.store(true, Ordering::SeqCst),
                    (false, true) => state.is_recording.store(false, Ordering::SeqCst),
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
}

impl RecorderState {
    fn get_segment_path(&self, segment: usize, suffix: &str) -> String {
        format!("{}/{}_{}{}", 
            self.output_directory,
            self.session_id,
            segment + self.starting_chunk,
            suffix
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
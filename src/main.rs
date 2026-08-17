#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(all(feature = "patcher", not(feature = "ui-check")))]
mod engine;
#[cfg(all(feature = "patcher", not(feature = "ui-check")))]
mod payload;

#[cfg(feature = "ui-check")]
mod engine {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Action {
        Apply,
        Restore,
    }

    #[derive(Clone, Debug)]
    pub struct Config {
        pub data_dir: PathBuf,
        pub action: Action,
        pub dry_run: bool,
    }

    pub fn discover_data_dir(_unused: &Path) -> PathBuf {
        PathBuf::from("UI 체크 모드")
    }

    pub fn resolve_data_dir(_path: &Path) -> Option<PathBuf> {
        None
    }

    pub fn validate(_config: &Config) -> Result<(), String> {
        Ok(())
    }

    pub fn run(config: Config, log: Arc<dyn Fn(String) + Send + Sync>) -> Result<(), String> {
        log("UI 체크 모드: 실제 파일은 변경되지 않습니다.".into());
        log(format!(
            "모의 작업: {}{} (대상: {})",
            match config.action {
                Action::Apply => "패치 적용",
                Action::Restore => "원본 복원",
            },
            if config.dry_run { " (dry-run)" } else { "" },
            config.data_dir.display()
        ));
        Ok(())
    }
}

use native_windows_gui as nwg;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use engine::{Action, Config};

const INFO_URL: &str = "https://github.com/gembleman/sister_other_paranoia_korean_patcher";
const VERSION: &str = "v1.0 by gemble";

#[derive(Default)]
struct PatcherUi {
    app_icon: nwg::Icon,
    window: nwg::Window,
    folder_heading: nwg::Label,
    folder_help: nwg::Label,
    data_input: nwg::TextInput,
    data_browse: nwg::Button,
    folder_status: nwg::Label,
    action_heading: nwg::Label,
    action_help: nwg::Label,
    dry_check: nwg::CheckBox,
    apply_button: nwg::Button,
    restore_button: nwg::Button,
    log_heading: nwg::Label,
    run_status: nwg::Label,
    log_box: nwg::TextBox,
    notice: nwg::Notice,
    dialog: nwg::FileDialog,
    footer_text: nwg::Label,
    info_link: nwg::Label,
}

impl PatcherUi {
    fn build() -> Result<Self, nwg::NwgError> {
        let mut ui = Self::default();
        let resources = nwg::EmbedResource::load(None)?;
        nwg::Icon::builder()
            .source_embed(Some(&resources))
            .source_embed_id(1)
            .size(Some((32, 32)))
            .build(&mut ui.app_icon)?;
        nwg::Window::builder()
            .size((800, 595))
            .center(true)
            .title(&format!("Sister Other Paranoia 한국어 패치 {VERSION}"))
            .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
            .icon(Some(&ui.app_icon))
            .build(&mut ui.window)?;

        nwg::Label::builder()
            .text("1. 게임 폴더 확인")
            .position((32, 32))
            .size((300, 30))
            .parent(&ui.window)
            .build(&mut ui.folder_heading)?;
        nwg::Label::builder()
            .text("게임 설치 폴더 또는 SisterOtherParanoia_Data 폴더를 선택하세요.")
            .position((34, 62))
            .size((650, 24))
            .parent(&ui.window)
            .build(&mut ui.folder_help)?;
        nwg::TextInput::builder()
            .position((32, 93))
            .size((616, 34))
            .parent(&ui.window)
            .build(&mut ui.data_input)?;
        nwg::Button::builder()
            .text("폴더 선택")
            .position((658, 92))
            .size((102, 36))
            .parent(&ui.window)
            .build(&mut ui.data_browse)?;
        nwg::Label::builder()
            .text("")
            .position((34, 131))
            .size((700, 25))
            .parent(&ui.window)
            .build(&mut ui.folder_status)?;

        nwg::Label::builder()
            .text("2. 작업 실행")
            .position((32, 170))
            .size((300, 30))
            .parent(&ui.window)
            .build(&mut ui.action_heading)?;
        nwg::Label::builder()
            .text("패치 적용 전 원본 파일은 자동으로 백업됩니다.")
            .position((34, 200))
            .size((500, 24))
            .parent(&ui.window)
            .build(&mut ui.action_help)?;
        nwg::CheckBox::builder()
            .text("파일을 변경하지 않고 호환성만 검사")
            .position((34, 230))
            .size((300, 28))
            .parent(&ui.window)
            .build(&mut ui.dry_check)?;

        nwg::Button::builder()
            .text("한국어 패치 적용")
            .position((32, 270))
            .size((470, 46))
            .parent(&ui.window)
            .build(&mut ui.apply_button)?;
        nwg::Button::builder()
            .text("원본으로 복원")
            .position((512, 270))
            .size((248, 46))
            .parent(&ui.window)
            .build(&mut ui.restore_button)?;
        nwg::Label::builder()
            .text("작업 기록")
            .position((32, 341))
            .size((88, 28))
            .parent(&ui.window)
            .build(&mut ui.log_heading)?;
        nwg::Label::builder()
            .text("준비됨")
            .position((120, 341))
            .size((180, 24))
            .parent(&ui.window)
            .build(&mut ui.run_status)?;
        nwg::TextBox::builder()
            .text("게임 폴더를 확인한 뒤 원하는 작업을 실행하세요.\r\n")
            .position((32, 375))
            .size((728, 130))
            .readonly(true)
            .parent(&ui.window)
            .build(&mut ui.log_box)?;
        nwg::Label::builder()
            .text("오역 제보나 업데이트 확인은")
            .position((4, 518))
            .size((210, 28))
            .parent(&ui.window)
            .build(&mut ui.footer_text)?;
        nwg::Label::builder()
            .text(INFO_URL)
            .position((220, 518))
            .size((600, 28))
            .parent(&ui.window)
            .build(&mut ui.info_link)?;
        nwg::Notice::builder()
            .parent(&ui.window)
            .build(&mut ui.notice)?;
        nwg::FileDialog::builder()
            .action(nwg::FileDialogAction::OpenDirectory)
            .title("폴더 선택")
            .build(&mut ui.dialog)?;

        let discovered = engine::discover_data_dir(std::path::Path::new(""));
        ui.data_input.set_text(&discovered.to_string_lossy());
        set_folder_status(&ui.folder_status, &discovered);
        Ok(ui)
    }
}

fn set_folder_status(label: &nwg::Label, path: &std::path::Path) {
    if engine::resolve_data_dir(path).is_some() {
        label.set_text("게임 데이터 폴더를 확인했습니다.");
    } else {
        label.set_text("게임을 찾지 못했습니다. [폴더 선택]을 눌러 직접 지정해 주세요.");
    }
}

fn append_log(box_control: &nwg::TextBox, line: &str) {
    let mut text = box_control.text();
    text.push_str(line);
    text.push_str("\r\n");
    // 로그가 무한히 커지는 것을 막되 최근 내용은 충분히 유지한다.
    if text.len() > 120_000 {
        let split = text
            .char_indices()
            .find(|(i, _)| *i >= 20_000)
            .map(|(i, _)| i)
            .unwrap_or(0);
        text.drain(..split);
    }
    box_control.set_text(&text);
    let end = box_control.len();
    box_control.set_selection(end..end);
    box_control.scroll_lastline();
}

fn find_malgun_gothic() -> Option<String> {
    let families = nwg::Font::families();
    families
        .into_iter()
        .find(|family| family.eq_ignore_ascii_case("Malgun Gothic") || family == "맑은 고딕")
}

fn configure_ui_font() -> Result<(), nwg::NwgError> {
    let family = find_malgun_gothic();
    let mut builder = nwg::Font::builder().size_absolute(16);
    if let Some(family) = family.as_deref() {
        builder = builder.family(family);
    }

    let mut font = nwg::Font::default();
    builder.build(&mut font)?;
    nwg::Font::set_global_default(Some(font));
    Ok(())
}

fn main() {
    nwg::init().expect("Win32 GUI 초기화 실패");
    configure_ui_font().expect("GUI 글꼴 설정 실패");
    let ui = Rc::new(PatcherUi::build().expect("GUI 생성 실패"));
    let messages = Arc::new(Mutex::new(Vec::<String>::new()));

    let window = ui.window.handle;
    let notice_sender = ui.notice.sender();
    let queue = messages.clone();
    let event_ui = ui.clone();
    let running = Arc::new(AtomicBool::new(false));
    let close_running = running.clone();

    let handler =
        nwg::full_bind_event_handler(&ui.window.handle, move |event, _evt_data, handle| {
            if event == nwg::Event::OnWindowClose && handle == window {
                if close_running.load(Ordering::Acquire) {
                    nwg::modal_error_message(
                        window,
                        "작업 진행 중",
                        "패치 작업이 끝난 뒤 창을 닫아 주세요.",
                    );
                } else {
                    nwg::stop_thread_dispatch();
                }
            } else if event == nwg::Event::OnButtonClick && handle == event_ui.data_browse.handle {
                if event_ui.dialog.run(Some(&window))
                    && let Ok(path) = event_ui.dialog.get_selected_item()
                {
                    let path = PathBuf::from(path);
                    let path = engine::resolve_data_dir(&path).unwrap_or(path);
                    event_ui.data_input.set_text(&path.to_string_lossy());
                    set_folder_status(&event_ui.folder_status, &path);
                }
            } else if event == nwg::Event::OnTextInput && handle == event_ui.data_input.handle {
                set_folder_status(
                    &event_ui.folder_status,
                    std::path::Path::new(&event_ui.data_input.text()),
                );
            } else if event == nwg::Event::OnLabelClick && handle == event_ui.info_link.handle {
                if let Err(error) = std::process::Command::new("rundll32")
                    .args(["url.dll,FileProtocolHandler", INFO_URL])
                    .spawn()
                {
                    nwg::modal_error_message(
                        window,
                        "링크 열기 실패",
                        &format!("{INFO_URL}\n\n{error}"),
                    );
                }
            } else if event == nwg::Event::OnButtonClick
                && (handle == event_ui.apply_button.handle
                    || handle == event_ui.restore_button.handle)
            {
                let config = Config {
                    data_dir: PathBuf::from(event_ui.data_input.text()),
                    action: if handle == event_ui.apply_button.handle {
                        Action::Apply
                    } else {
                        Action::Restore
                    },
                    dry_run: event_ui.dry_check.check_state() == nwg::CheckBoxState::Checked,
                };
                if let Err(error) = engine::validate(&config) {
                    nwg::modal_error_message(window, "입력 확인", &error);
                    return;
                }
                event_ui.apply_button.set_enabled(false);
                event_ui.restore_button.set_enabled(false);
                event_ui.data_browse.set_enabled(false);
                event_ui.data_input.set_enabled(false);
                event_ui.dry_check.set_enabled(false);
                event_ui.run_status.set_text(if config.dry_run {
                    "호환성 검사 중"
                } else if config.action == Action::Apply {
                    "패치 적용 중"
                } else {
                    "원본 복원 중"
                });
                running.store(true, Ordering::Release);
                append_log(
                    &event_ui.log_box,
                    if config.action == Action::Apply {
                        "===== 패치 시작 ====="
                    } else {
                        "===== 복원 시작 ====="
                    },
                );
                let worker_queue = queue.clone();
                let sender = notice_sender;
                let worker_running = running.clone();
                thread::spawn(move || {
                    let q = worker_queue.clone();
                    let n = sender;
                    let logger: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |line| {
                        q.lock().unwrap().push(line);
                        n.notice();
                    });
                    let result = engine::run(config, logger);
                    worker_running.store(false, Ordering::Release);
                    worker_queue.lock().unwrap().push(match result {
                        Ok(()) => "__DONE_OK__".into(),
                        Err(e) => format!("__DONE_ERR__{e}"),
                    });
                    sender.notice();
                });
            } else if event == nwg::Event::OnNotice && handle == event_ui.notice.handle {
                let drained: Vec<String> = messages.lock().unwrap().drain(..).collect();
                for message in drained {
                    if message == "__DONE_OK__" {
                        append_log(&event_ui.log_box, "===== 모든 작업 완료 =====");
                        event_ui.apply_button.set_enabled(true);
                        event_ui.restore_button.set_enabled(true);
                        event_ui.data_browse.set_enabled(true);
                        event_ui.data_input.set_enabled(true);
                        event_ui.dry_check.set_enabled(true);
                        event_ui.run_status.set_text("완료");
                        nwg::modal_info_message(window, "완료", "선택한 작업이 완료되었습니다.");
                    } else if let Some(error) = message.strip_prefix("__DONE_ERR__") {
                        append_log(&event_ui.log_box, &format!("[실패] {error}"));
                        event_ui.apply_button.set_enabled(true);
                        event_ui.restore_button.set_enabled(true);
                        event_ui.data_browse.set_enabled(true);
                        event_ui.data_input.set_enabled(true);
                        event_ui.dry_check.set_enabled(true);
                        event_ui.run_status.set_text("작업 실패");
                        nwg::modal_error_message(window, "작업 실패", error);
                    } else {
                        append_log(&event_ui.log_box, &message);
                    }
                }
            }
        });
    nwg::dispatch_thread_events();
    nwg::unbind_event_handler(&handler);
}

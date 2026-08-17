use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::payload;

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
    let mut candidates = Vec::new();
    if let Ok(value) = env::var("SOP_GAME_DATA") {
        candidates.push(PathBuf::from(value));
    }
    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join("SisterOtherParanoia_Data"));
        if let Some(parent) = cwd.parent() {
            candidates.push(parent.join("SisterOtherParanoia_Data"));
        }
    }
    if let Ok(exe) = env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("SisterOtherParanoia_Data"));
        if let Some(parent) = dir.parent() {
            candidates.push(parent.join("SisterOtherParanoia_Data"));
        }
    }
    candidates
        .into_iter()
        .find(|p| p.join("resources.assets").is_file())
        .unwrap_or_else(|| PathBuf::from("SisterOtherParanoia_Data"))
}

/// 사용자가 Unity 데이터 폴더 자체 또는 게임 설치 루트를 선택할 수 있게 한다.
pub fn resolve_data_dir(path: &Path) -> Option<PathBuf> {
    if path.join("resources.assets").is_file() {
        return Some(path.to_path_buf());
    }

    let data_dir = path.join("SisterOtherParanoia_Data");
    data_dir
        .join("resources.assets")
        .is_file()
        .then_some(data_dir)
}

pub fn validate(config: &Config) -> Result<(), String> {
    if resolve_data_dir(&config.data_dir).is_none() {
        return Err(format!(
            "올바른 게임 폴더 또는 데이터 폴더가 아닙니다: {}",
            config.data_dir.display()
        ));
    }
    payload::load_manifest().map(|_| ())
}

pub fn run(mut config: Config, log: Arc<dyn Fn(String) + Send + Sync>) -> Result<(), String> {
    validate(&config)?;
    config.data_dir = resolve_data_dir(&config.data_dir)
        .ok_or_else(|| "게임 데이터 폴더에 더 이상 접근할 수 없습니다.".to_string())?;
    let manifest = payload::load_manifest()?;
    let files = payload::files_for(manifest);
    if files.is_empty() {
        return Err("내장 payload가 없습니다.".into());
    }

    log(format!("게임 데이터: {}", config.data_dir.display()));
    log(format!("payload 파일 {}개", files.len()));
    log("\r\n전체 파일 호환성 검사 및 staging".into());
    let total = payload::apply_transaction(
        &config.data_dir,
        files,
        config.action == Action::Restore,
        config.dry_run,
        log.as_ref(),
    )?;
    if config.dry_run {
        log(format!("변경 대상 {total}개 확인 (dry-run)"));
    } else {
        log(format!("트랜잭션 커밋 완료 ({total}개)"));
    }
    log(format!("\r\n총 변경 항목: {total}개"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_data_dir;
    use std::fs;
    use std::path::PathBuf;

    struct TestDir(PathBuf);

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_game_root_to_unity_data_folder() {
        let root = std::env::temp_dir().join(format!(
            "sop_korean_patcher_resolve_test_{}",
            std::process::id()
        ));
        let test_dir = TestDir(root);
        let data_dir = test_dir.0.join("SisterOtherParanoia_Data");
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(data_dir.join("resources.assets"), []).unwrap();

        assert_eq!(resolve_data_dir(&test_dir.0), Some(data_dir.clone()));
        assert_eq!(resolve_data_dir(&data_dir), Some(data_dir));
    }
}

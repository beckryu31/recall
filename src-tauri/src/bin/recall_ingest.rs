//! 헤드리스 수집기. launchd 가 매시간 돌린다.
//!
//! Recall 앱이 닫혀 있어도 응답이 수집된다. 앱이 열려 있을 때는 앱이 직접 수집하지만,
//! **둘은 같은 `ingest.rs` 를 쓴다** — 추출 로직이 두 벌이 되는 순간 그것들은 조용히
//! 갈라지고, 그게 프롬프트 438개를 죽인 사고의 형태다.
//!
//! Claude Code 에는 아무 지연도 추가하지 않는다. Stop 훅이 없기 때문이다.
//! (실측: Stop 은 중단된 턴 9.9%에서 발생하지 않아 어차피 스위퍼가 필요하고,
//!  스위퍼가 있으면 훅이 사는 건 지연시간뿐이며, 391MB 를 몇 초에 훑는다.)

fn main() {
    // `install.sh` 가 **설치된 이 바이너리에게 직접** 버전을 묻는다. 레포의 버전 문자열을
    // 그대로 딱지로 찍으면, 수집기를 다시 빌드하지 않은 채 install.sh 만 돌렸을 때
    // **낡은 바이너리에 새 딱지**가 붙는다. 거짓말하는 표시는 없느니만 못하다.
    if std::env::args().any(|a| a == "--version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return;
    }

    match recall_lib::run_ingest() {
        Ok(s) => {
            println!(
                "수집 완료 · 세션 {} (기록 없음 {}) · 프롬프트 {} 매칭 / {} 미해결 · 세그먼트 {} · 접힌 알림 {}",
                s.sessions, s.sessions_missing, s.prompts_matched, s.prompts_unresolved,
                s.segments, s.folded
            );
        }
        Err(e) => {
            eprintln!("수집 실패: {e}");
            std::process::exit(1);
        }
    }
}

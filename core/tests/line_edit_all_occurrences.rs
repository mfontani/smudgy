//! The text-search line edits (`line.replace`, `line.remove`, `line.highlight`) act on
//! EVERY occurrence of the search string, not just the first. The replace case grows the
//! text and puts a multi-byte character before the first match, so it fails loudly if the
//! occurrences are spliced in the wrong order or addressed by UTF-16 offsets. Runs through
//! the real session runtime, exactly like `line_replace_non_ascii.rs`.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use smudgy_core::session::connection::vt_processor::{AnsiColor, Color};
use smudgy_core::session::runtime::RuntimeAction;
use smudgy_core::session::styled_line::{Style, StyledLine, VtSpan};
use smudgy_core::session::{BufferUpdate, SessionEvent, SessionId, SessionParams, spawn};

const QUIET_PERIOD: Duration = Duration::from_millis(900);

/// Triggers that edit in-place and echo nothing, so each edited incoming line is emitted
/// straight to the buffer for the test to read back.
const LINE_EDIT_ALL_TS: &str = r#"
import { createTrigger, echo, line } from "smudgy:core";

createTrigger("^REP ", () => {
    if (!line.replace("orc", "<orc!>")) echo("REP_MISS");
});

createTrigger("^RM ", () => {
    if (!line.remove(" gone")) echo("RM_MISS");
});

createTrigger("^HL ", () => {
    if (!line.highlight("orc", { fg: "red" })) echo("HL_MISS");
});

echo("EDITALL_READY");
"#;

/// A plain incoming server line: one default-styled span over the whole (byte-length) text.
fn plain_line(text: &str) -> Arc<StyledLine> {
    let span = VtSpan {
        style: Style {
            fg: Color::DefaultForeground { bold: false },
            bg: Color::DefaultBackground,
            ..Style::DEFAULT
        },
        begin_pos: 0,
        end_pos: text.len(),
    };
    Arc::new(StyledLine::new(text, vec![span]))
}

#[tokio::test]
async fn line_text_search_edits_apply_to_every_occurrence() {
    let home = tempfile::tempdir().expect("create temp home");
    let home_path = home.path().to_path_buf();
    std::mem::forget(home);
    smudgy_core::set_smudgy_home(&home_path);
    let home_path = smudgy_core::get_smudgy_home().expect("smudgy home");

    let server = "LineEditAllOccurrences";
    let modules_dir = home_path.join(server).join("modules");
    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(home_path.join(server).join("logs")).unwrap();
    std::fs::write(modules_dir.join("line_edit_all.ts"), LINE_EDIT_ALL_TS).unwrap();

    let params = Arc::new(SessionParams {
        session_id: SessionId::from(7008),
        server_name: Arc::new(server.to_string()),
        profile_name: Arc::new("Test".to_string()),
        profile_subtext: Arc::new(String::new()),
        mapper: None,
        package_client: None,
        extra_script_extensions: Arc::new(Vec::new),
        on_engine_rebuild: None,
    });

    let mut events = Box::pin(spawn(params));

    let tx = loop {
        let event = tokio::time::timeout(Duration::from_mins(1), events.next())
            .await
            .expect("timed out waiting for RuntimeReady")
            .expect("event stream ended before RuntimeReady");
        if let SessionEvent::RuntimeReady(tx) = event.event {
            break tx;
        }
    };

    // The multi-byte `caf\u{e9}` shifts bytes ahead of every match, and the replacement is
    // longer than the needle, so left-to-right splicing on stale offsets would mangle the
    // second and third occurrences.
    let rep_incoming = "REP caf\u{e9} orc and orc and orc";
    let rep_expected = "REP caf\u{e9} <orc!> and <orc!> and <orc!>";
    let rm_incoming = "RM one gone two gone three gone";
    let rm_expected = "RM one two three";
    let hl_incoming = "HL an orc and an orc";

    let mut lines: Vec<Arc<StyledLine>> = Vec::new();
    let mut sent = false;
    loop {
        let Ok(Some(event)) = tokio::time::timeout(QUIET_PERIOD, events.next()).await else {
            break;
        };
        if let SessionEvent::UpdateBuffer(updates) = event.event {
            for update in updates.iter() {
                if let BufferUpdate::Append(line) = update {
                    lines.push(line.clone());
                    if !sent && line.text == "EDITALL_READY" {
                        for incoming in [rep_incoming, rm_incoming, hl_incoming] {
                            tx.send(RuntimeAction::HandleIncomingLine(plain_line(incoming)))
                                .unwrap();
                        }
                        tx.send(RuntimeAction::RequestRepaint).unwrap();
                        sent = true;
                    }
                }
            }
        }
    }

    tx.send(RuntimeAction::Shutdown).ok();

    let transcript = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        lines.iter().any(|l| l.text == rep_expected),
        "line.replace must replace every occurrence, splicing right to left.\n\
         expected: {rep_expected:?}\nTranscript:\n{transcript}"
    );
    assert!(
        lines.iter().any(|l| l.text == rm_expected),
        "line.remove must remove every occurrence.\n\
         expected: {rm_expected:?}\nTranscript:\n{transcript}"
    );

    // The highlight line keeps its text; every "orc" range must have been recolored red.
    let highlighted = lines
        .iter()
        .find(|l| l.text == hl_incoming)
        .unwrap_or_else(|| {
            panic!("the HL line must still be delivered.\nTranscript:\n{transcript}")
        });
    let red_texts: Vec<&str> = highlighted
        .spans
        .iter()
        .filter(|span| {
            matches!(
                span.style.fg,
                Color::Ansi {
                    color: AnsiColor::Red,
                    ..
                }
            )
        })
        .map(|span| &highlighted.text[span.begin_pos..span.end_pos])
        .collect();
    assert_eq!(
        red_texts,
        ["orc", "orc"],
        "line.highlight must recolor every occurrence.\nTranscript:\n{transcript}"
    );
}

// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Welcome screen renders the Doggy half-block brand logo correctly.
///
/// Doggy brand uses block elements (▀▄█). A regression in the writer
/// thread (using `WriteFile` instead of `WriteConsoleW` on Windows, or a
/// missing `SetConsoleOutputCP(65001)`) would garble multi-byte UTF-8.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn welcome_screen_braille_logo_renders_correctly() {
    let content = ContentController::start().await.expect("start content");

    let binary = pager_binary().expect("resolve pager binary");
    // Tall terminal so the portrait tier is selected.
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    let screen = harness.screen_contents();

    // Doggy logo is half-block art (▀ / ▄ / █) or the DOGGY wordmark on
    // short / legacy consoles.
    let has_blocks = screen.contains('▀') || screen.contains('▄') || screen.contains('█');
    let has_wordmark = screen.contains("DOGGY");
    assert!(
        has_blocks || has_wordmark,
        "Doggy brand logo not found (expected half-block art or DOGGY wordmark).\n\
         Screen contents:\n{screen}"
    );

    harness.quit().expect("clean quit");
}

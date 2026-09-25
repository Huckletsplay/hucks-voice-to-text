//! Check for Updates, against the real GitHub release.
//!
//! `#[ignore]`d: it needs the network and downloads the full DMG (~133 MB). Run on purpose after
//! publishing a release: `scripts/dev.sh test -- --ignored update_live`.

use hvtt_desktop::update::{self, Check};

#[test]
#[ignore]
fn update_live_the_published_release_is_found_downloaded_and_verified() {
    // Pretend to be an old copy, so the latest release counts as new.
    let Check::Available(offer) = update::fetch_latest("0.0.1").expect("GitHub answered") else {
        panic!("the latest release should be offered to 0.0.1");
    };
    let dmg = update::download(&offer).expect("the DMG downloads and matches its checksum");
    assert_eq!(std::fs::metadata(&dmg).unwrap().len(), offer.dmg_size);
    let _ = std::fs::remove_dir_all(update::download_dir());

    // The copy that is the latest release is told so.
    assert!(matches!(update::fetch_latest(&offer.version).unwrap(), Check::UpToDate { .. }));
}

//! The speech models he can choose between: H › Settings › Speech Model.
//!
//! Chosen by measurement, 2026-09-30 (`desktop/examples/accuracy.rs`: 164 quick phrases, twelve
//! voices at fast speaking rates, on an M3 Pro's graphics processor). Share of words wrong, and
//! the time from the stop press to the words for a phrase of a second or two:
//!
//! | model | clean | under loud hiss (12 dB) | time |
//! |---|---|---|---|
//! | base.en ("Quick") | 6.2% | 15.7% | 0.08 s |
//! | small.en ("Better") | 3.2% | 11.8% | 0.22 s |
//! | large-v3-turbo, 5-bit ("Best") | 1.9% | 10.5% | 0.9 s |
//!
//! Its 8-bit version was no more accurate (1.9%) and is 300 MB bigger. The Windows build
//! recognises on the processor alone, and there the same phrase takes (measured 2026-10-03 on a
//! six-core i5-11400F, 11 threads, built for that processor): Quick 1.25 s, Better 4.1 s, Best
//! 20 s.
//!
//! One model is inside the program - "Best" (large-v3-turbo) on the Mac, "Quick" on Windows
//! (`MAC`). The others are downloaded when he chooses one - the whisper.cpp project's own files,
//! pinned to one commit - and kept only if the download's size and SHA-256 are exactly the ones
//! written here.

/// One model on offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    /// The file in the models folder, which is also what settings remember.
    pub file: &'static str,
    /// What the menu calls it.
    pub name: &'static str,
    /// What choosing it means, in plain words.
    pub note: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    /// Shipped inside the program: never downloaded.
    pub built_in: bool,
    /// Out of five, from the measurements above - shown as dots in the menu, as Handy's list did.
    pub accuracy: u8,
    pub speed: u8,
    /// One quick phrase's recognition on the M3 Pro, in ms (2026-10-02) - to estimate another
    /// model's time from this computer's measured one (`faster_choice`).
    pub mac_ms: u32,
}

/// The whisper.cpp project's models, at one commit, so a file can never change under the same
/// name.
const SOURCE: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/";

/// On the Mac, "Best" is the one inside the program (his decision 2026-10-02: "accuracy gives more
/// confidence"; anyone wanting speed picks Better or Quick). Quick phrases 1.9% of words wrong
/// against Better's 3.2% and Quick's 6.2%, and a clipped first word 14% against 21-23%, for about
/// 0.9 s at the stop on an M3 Pro (~2 s estimated on an M1) and ~770 MB of memory. (Better was the
/// default for a few hours the same day.)
///
/// **On Windows, "Quick" is the one inside.** Best was Windows' too from 2026-10-02 - his call:
/// "Windows will be Best as well until I test it myself", to be measured on the PC and changed
/// back to Quick there if it was too slow. Measured 2026-10-03: there it recognises on the
/// processor alone, and a quick phrase took 20 s with Best against 1.25 s with Quick (4.1 s with
/// Better). Best is still his to choose from the menu.
const MAC: bool = cfg!(target_os = "macos");

pub const MODELS: &[Model] = &[
    Model {
        file: "ggml-tiny.en.bin",
        name: "Tiny",
        note: "for very old computers",
        bytes: 77_704_715,
        sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
        built_in: false,
        accuracy: 1,
        speed: 5,
        mac_ms: 46,
    },
    Model {
        file: "ggml-base.en.bin",
        name: "Quick",
        note: "very fast, more mistakes",
        bytes: 147_964_211,
        sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
        built_in: !MAC,
        accuracy: 2,
        speed: 5,
        mac_ms: 79,
    },
    Model {
        file: "ggml-small.en.bin",
        name: "Better",
        note: "faster, a few more mistakes",
        bytes: 487_614_201,
        sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
        built_in: false,
        accuracy: 3,
        speed: 4,
        mac_ms: 222,
    },
    Model {
        file: "ggml-medium.en-q5_0.bin",
        name: "Medium",
        note: "nearly Best, a little quicker",
        bytes: 539_225_533,
        sha256: "76733e26ad8fe1c7a5bf7531a9d41917b2adc0f20f2e4f5531688a8c6cd88eb0",
        built_in: false,
        accuracy: 4,
        speed: 3,
        mac_ms: 619,
    },
    Model {
        file: "ggml-large-v3-turbo-q5_0.bin",
        name: "Best",
        note: if MAC { "fewest mistakes" } else { "fewest mistakes, very slow on most PCs" },
        bytes: 574_041_195,
        sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
        built_in: MAC,
        accuracy: 5,
        speed: 2,
        mac_ms: 911,
    },
    Model {
        file: "ggml-large-v3-q5_0.bin",
        name: "Large",
        note: "slowest; best with heavy background noise in our tests",
        bytes: 1_081_140_203,
        sha256: "d75795ecff3f83b5faa89d1900604ad8c780abd5739fae406de19f23ecd98ad1",
        built_in: false,
        accuracy: 5,
        speed: 1,
        mac_ms: 1161,
    },
];

/// Not a speech model to choose: whisper.cpp's voice detector (Silero v5.1.2, MIT), which tells
/// quiet speech from noise and finds where speech ends. Shipped inside the program beside the
/// built-in model; without it, loudness alone decides. From
/// `https://huggingface.co/ggml-org/whisper-vad/resolve/9ffd54a1e1ee413ddf265af9913beaf518d1639b/`.
pub const VOICE_DETECTOR: Model = Model {
    file: "ggml-silero-v5.1.2.bin",
    name: "Voice detector",
    note: "built in",
    bytes: 885_098,
    sha256: "29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf",
    built_in: true,
    accuracy: 0,
    speed: 0,
    mac_ms: 0,
};

/// What the speed check (`desktop` `speed_check`) makes of one full recognition on this computer -
/// roughly the wait after every stop press. His idea, 2026-10-02: "run a system test to see what
/// it would be good for and then automatically set the settings."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeedAdvice {
    /// Quick enough: nothing changes.
    Fine,
    /// A little slow: the words shown while he talks refresh less often (Live Words "Lighter").
    Lighter,
    /// Too slow for comfort: a smaller model is offered - never switched to unasked, as it means a
    /// download, and the program goes online only when he asks.
    Smaller,
}

/// Up to 1.5 s fine; up to 3 s lighter live words; beyond, offer a smaller model.
pub fn speed_advice(recognition_ms: u128) -> SpeedAdvice {
    match recognition_ms {
        0..=1_500 => SpeedAdvice::Fine,
        1_501..=3_000 => SpeedAdvice::Lighter,
        _ => SpeedAdvice::Smaller,
    }
}

/// The model to offer when this computer is slow with `current` (`measured_ms` here): the most
/// accurate one estimated to take at most 1.5 s, by its measured time relative to `current`'s -
/// not simply the fastest, which gives up the most accuracy (Codex's seventeenth review). Never a
/// model estimated slower than `current`. With none that quick, the fastest that is quicker.
pub fn faster_choice(current: &Model, measured_ms: u128) -> Option<(&'static Model, u128)> {
    let estimate = |m: &Model| measured_ms * m.mac_ms as u128 / current.mac_ms.max(1) as u128;
    let quicker: Vec<&'static Model> = MODELS.iter().filter(|m| m.mac_ms < current.mac_ms).collect();
    quicker
        .iter()
        .filter(|m| estimate(m) <= 1_500)
        .max_by_key(|m| (m.accuracy, std::cmp::Reverse(m.mac_ms)))
        .or_else(|| quicker.iter().min_by_key(|m| m.mac_ms))
        .map(|m| (*m, estimate(m)))
}

/// The model a settings file names, if it is one of these.
pub fn find(file: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.file == file)
}

/// The one inside the program, which is always there to fall back on.
pub fn built_in() -> &'static Model {
    MODELS.iter().find(|m| m.built_in).expect("one model ships inside the program")
}

pub fn download_url(model: &Model) -> String {
    format!("{SOURCE}{}", model.file)
}

/// "490 MB", as a download is usually described.
pub fn megabytes(bytes: u64) -> String {
    format!("{} MB", (bytes + 5_000_000) / 10_000_000 * 10)
}

/// Five dots, `n` of them filled.
fn dots(n: u8) -> String {
    (1..=5).map(|i| if i <= n { '●' } else { '○' }).collect()
}

/// The menu line: its name, how accurate and how fast it is (dots, out of five, as Handy's list
/// showed), what it is for, and what choosing it would cost him right now.
pub fn menu_label(model: &Model, on_this_computer: bool, downloading: bool) -> String {
    let rating = format!("{} accurate  {} fast", dots(model.accuracy), dots(model.speed));
    // Its size always shows: it is what the model takes on the computer (his request, 2026-10-02).
    let size = megabytes(model.bytes);
    let cost = if downloading {
        format!("downloading {size}…")
    } else if model.built_in {
        format!("built in · {size}")
    } else if on_this_computer {
        format!("downloaded · {size}")
    } else {
        format!("{size} download")
    };
    format!("{} — {}  ·  {rating}  ·  {cost}", model.name, model.note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_model_ships_inside_and_it_is_the_default() {
        assert_eq!(MODELS.iter().filter(|m| m.built_in).count(), 1);
        assert_eq!(built_in().file, crate::settings::Settings::default().model);
    }

    #[test]
    fn every_download_is_pinned_to_one_commit_over_https() {
        for m in MODELS {
            let url = download_url(m);
            assert!(url.starts_with("https://huggingface.co/ggerganov/whisper.cpp/resolve/"));
            assert!(!url.contains("/resolve/main/"), "a branch can change under the same name: {url}");
            assert!(url.ends_with(m.file));
            assert_eq!(m.sha256.len(), 64);
            assert!(m.sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
            assert!(m.bytes > 50_000_000, "{} looks too small to be a model", m.file);
        }
    }

    #[test]
    fn the_menu_says_what_a_choice_would_download() {
        let better = find("ggml-small.en.bin").unwrap();
        let line = menu_label(better, false, false);
        assert!(line.starts_with("Better — faster, a few more mistakes"), "{line}");
        assert!(line.contains("●●●○○ accurate  ●●●●○ fast"), "{line}");
        assert!(line.ends_with("490 MB download"), "{line}");
        assert!(menu_label(better, true, false).ends_with("downloaded · 490 MB"));
        assert!(menu_label(better, false, true).ends_with("downloading 490 MB…"));
        // "Best" on the Mac; "Quick" where recognition is on the processor alone.
        let inside = built_in();
        let (name, size) = if MAC { ("Best", "570 MB") } else { ("Quick", "150 MB") };
        assert_eq!(inside.name, name);
        assert!(menu_label(inside, false, false).ends_with(&format!("built in · {size}")));
    }

    #[test]
    fn the_speed_check_changes_nothing_on_a_quick_computer_and_offers_help_on_a_slow_one() {
        assert_eq!(speed_advice(900), SpeedAdvice::Fine, "Best on this Mac");
        assert_eq!(speed_advice(2_200), SpeedAdvice::Lighter);
        assert_eq!(speed_advice(9_000), SpeedAdvice::Smaller, "Best on a slow laptop's processor");
    }

    #[test]
    fn a_slow_computer_is_offered_the_most_accurate_model_quick_enough() {
        let best = find("ggml-large-v3-turbo-q5_0.bin").unwrap();
        // Best at 5 s: Better (222/911 of it, about 1.2 s) is the most accurate within 1.5 s.
        let (offer, ms) = faster_choice(best, 5_000).unwrap();
        assert_eq!(offer.name, "Better", "not Quick: Better makes half its mistakes");
        assert!((1_100..1_300).contains(&ms), "{ms}");
        // Very slow: the fastest that is quicker.
        assert_eq!(faster_choice(best, 60_000).unwrap().0.name, "Tiny");
        // Nothing is quicker than Tiny, and Quick is never offered from Tiny.
        assert!(faster_choice(find("ggml-tiny.en.bin").unwrap(), 9_000).is_none());
    }

    #[test]
    fn a_model_not_on_offer_is_not_found() {
        assert!(find("ggml-medium.en.bin").is_none());
        assert_eq!(find("ggml-base.en.bin").map(|m| m.name), Some("Quick"));
    }
}

"""Spoken phrases for `desktop/examples/accuracy.rs` (`scripts/dev.sh bench`). macOS only.

Quick dictations the way they are really made: pressed, spoke at once, pressed again. Short
phrases said by the system's own voices at fast speaking rates, trimmed to 30 ms either side of
the speech, and written at 48 kHz like a real microphone - no microphone involved.

    python3 scripts/make-accuracy-corpus.py <out-dir>          # 164 quick phrases, 1-2 s each
    python3 scripts/make-accuracy-corpus.py <out-dir> --long   # 40 stretches of 4-12 s
    python3 scripts/make-accuracy-corpus.py <out-dir> --minute # 10 dictations of 30-60 s
    python3 scripts/make-accuracy-corpus.py <out-dir> --pauses # 20 dictations with thinking pauses

Needs `say` and `ffmpeg`, and numpy. Writes NNN.wav and manifest.tsv (file, voice, words a
minute, what was said). The same seed gives the same corpus every time.
"""
import os
import random
import re
import subprocess
import sys
import wave

import numpy as np

PHRASES = [
    "Sounds good, talk to you later.",
    "Can you send me the file when you get a chance?",
    "I'm running a little late, be there soon.",
    "Push the fix to GitHub tonight.",
    "Let's ship it tomorrow morning.",
    "Check the render before you post it.",
    "Thanks, that works for me.",
    "Open the settings menu and turn on learning.",
    "The build failed again on Windows.",
    "Remind me to call the dentist.",
    "What time does the store close?",
    "Please rename the folder and commit the change.",
    "Add milk and eggs to the grocery list.",
    "I think the clip is too long.",
    "Make the text box follow the newest words.",
    "Can you review this before lunch?",
    "That's weird, it worked yesterday.",
    "Move the meeting to Friday afternoon.",
    "Great job on the video.",
    "Turn the volume down a little.",
    "Delete the old drafts folder.",
    "We need to fix the paste in Chrome.",
    "Send it to the team channel.",
    "Did you get my last message?",
    "Start the download and let me know.",
    "The microphone keeps cutting out.",
    "Write a quick summary of the changes.",
    "No, the other one.",
    "Yes, go ahead.",
    "Hold on, let me check.",
    "Copy that into the notes.",
    "I'll look at it after dinner.",
    "Save the project and close the window.",
    "Why is the update not showing up?",
    "Pick up the kids at school.",
    "Put the thumbnail in the videos folder.",
    "Try the bigger speech model.",
    "Keep it short and simple.",
    "The words were wrong again.",
    "Record a new clip for the channel.",
    "Huck's Voice to Text is working now.",
]
VOICES = ["Samantha", "Daniel", "Karen", "Moira", "Tessa", "Rishi",
          "Reed (English (US))", "Flo (English (US))", "Eddy (English (US))",
          "Sandy (English (US))", "Shelley (English (US))", "Rocko (English (US))"]
RATE = 48_000
MARGIN = int(0.03 * RATE)  # 30 ms either side of the speech


def speak(text, voice, words_a_minute, out, tmp):
    aiff, raw = os.path.join(tmp, "tmp.aiff"), os.path.join(tmp, "tmp48.wav")
    subprocess.run(["say", "-v", voice, "-r", str(words_a_minute), "-o", aiff, text], check=True)
    subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-i", aiff, "-af",
                    f"aresample={RATE}:filter_size=64:phase_shift=10", "-ac", "1",
                    "-c:a", "pcm_s16le", raw], check=True)
    with wave.open(raw) as w:
        x = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16)
    level = np.abs(x.astype(np.float32))
    loud = np.nonzero(level > 0.02 * level.max())[0]
    x = x[max(0, loud[0] - MARGIN): loud[-1] + MARGIN]
    with wave.open(out, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(x.tobytes())
    for f in (aiff, raw):
        os.remove(f)
    return len(x) / RATE


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    out_dir, long, minute = sys.argv[1], "--long" in sys.argv[2:], "--minute" in sys.argv[2:]
    pauses = "--pauses" in sys.argv[2:]
    os.makedirs(out_dir, exist_ok=True)
    rows, secs = [], []
    if pauses:
        # Thoughts said in bursts, with the 1-2 s silences of someone thinking between them.
        # `say` makes the silences itself ([[slnc ms]]); the words compared against leave them out.
        rng = random.Random(31)
        jobs = []
        for n in range(20):
            groups = [" ".join(rng.sample(PHRASES, rng.randint(1, 3))) for _ in range(rng.randint(3, 5))]
            spoken = "".join(f"{g} [[slnc {rng.randint(1200, 2000)}]] " for g in groups[:-1]) + groups[-1]
            jobs.append((spoken, VOICES[n % len(VOICES)], rng.randint(220, 270)))
    elif minute:
        # One voice per dictation, as one person talks; phrases joined into sentences with the
        # pauses a speaker leaves between them.
        rng = random.Random(23)
        jobs = []
        for n in range(10):
            text = " ".join(rng.sample(PHRASES, len(PHRASES))[:rng.randint(22, 34)])
            jobs.append((text, VOICES[n % len(VOICES)], rng.randint(220, 270)))
    elif long:
        rng = random.Random(11)
        jobs = []
        for n in range(40):
            text = " ".join(rng.sample(PHRASES, rng.randint(3, 6)))
            jobs.append((text, VOICES[n % len(VOICES)], rng.randint(230, 290)))
    else:
        rng = random.Random(7)
        jobs = [(text, VOICES[(i + 3 * k) % len(VOICES)], rng.randint(240, 300))
                for i, text in enumerate(PHRASES) for k in range(4)]
    for n, (text, voice, wpm) in enumerate(jobs):
        name = f"{n:03d}.wav"
        secs.append(speak(text, voice, wpm, os.path.join(out_dir, name), out_dir))
        said = re.sub(r"\s*\[\[slnc \d+\]\]\s*", " ", text).strip()
        rows.append(f"{name}\t{voice}\t{wpm}\t{said}")
    with open(os.path.join(out_dir, "manifest.tsv"), "w") as f:
        f.write("\n".join(rows) + "\n")
    print(f"{len(rows)} clips, {min(secs):.2f}-{max(secs):.2f} s, in {out_dir}")


if __name__ == "__main__":
    main()

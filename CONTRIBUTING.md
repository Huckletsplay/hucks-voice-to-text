# Contributing

Thanks for helping improve Huck's Voice to Text.

## Before opening a change

1. Search existing issues and pull requests.
2. Keep platform-specific code behind the delivery and clipboard seams in
   `app/desktop/src/destination/` and `app/desktop/src/clip.rs`. `app/core/` stays free of
   platform code, so its tests run anywhere.
3. Avoid unrelated refactors in the same pull request.
4. Never include recordings, transcripts, recovery drafts, speech models, settings files,
   credentials, signing material, personal paths, or generated artifacts.

## Validate your change

```bash
app/scripts/dev.sh test
```

For recognition, delivery, clipboard, shortcut, permission, packaging, or updater changes, describe
the hands-on checks you ran — which apps you dictated into, and the macOS version and Mac used. A
passing unit suite does not replace trying dictation in real applications.

The two tests that use the real macOS clipboard are skipped by default; run them on purpose with
`app/scripts/dev.sh test -- --ignored`. They restore whatever was on the clipboard.

## Pull requests

Explain the user-visible problem, the approach taken, tests performed, and any limitations. Keep
public discussion focused on the software; do not include private conversations, development
journals, dictated text, or unrelated project material.

Changes to `hvtt_core::pipeline::complete_transcription` must keep its order — recovery draft,
then clipboard, then delivery — and the tests that pin it down. Never losing dictated text is the
one rule the program does not trade away.

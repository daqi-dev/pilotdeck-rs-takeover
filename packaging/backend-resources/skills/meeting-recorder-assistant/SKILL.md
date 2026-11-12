---
name: meeting-recorder-assistant
description: "Turn a supplied recording or transcript into attributed minutes and action items."
---

# meeting-recorder-assistant

For audio, use the audio-transcription skill and retain timestamps. Live recording requires an available recorder and explicit permission; this package does not capture a microphone. From the transcript, separate decisions, proposals, unresolved questions and assigned actions. Every action should retain its owner and due date when actually stated; leave them unspecified otherwise. Link uncertain passages to timestamps and do not invent agreement. The bundled transcript helper can normalize an existing SRT file; semantic minutes are written from that evidence.

For existing SRT input:

```sh
python3 {{SKILL_ROOT_SHELL}}/scripts/srt_transcript.py "$INPUT_SRT"
```

This only normalizes timestamps and words. It does not record, identify speakers or generate semantic action items.

The supplied FunASR bridge additionally requires Python 3 on POSIX for owned process-group cleanup. Its current Windows execution is explicitly unavailable; do not substitute an unsafe child-only launcher or claim successful transcription.

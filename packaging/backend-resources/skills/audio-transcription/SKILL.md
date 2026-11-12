---
name: audio-transcription
description: "Transcribe a project-local audio file and produce timestamped text or subtitles."
---

# audio-transcription

Use the funasr MCP transcribe_audio tool with audio_path and language auto. The file must resolve inside the active project; copy an authorized external input into the project first. Keep the returned segment times with their original words. Do not infer speaker identities or fill inaudible speech. A usable local SenseVoice executable plus acoustic and VAD models must be provisioned separately. If unavailable, report the actual tool error; never invent a transcript.

The supplied FunASR bridge additionally requires Python 3 on POSIX for owned process-group cleanup. Its current Windows execution is explicitly unavailable; do not substitute an unsafe child-only launcher or claim successful transcription.

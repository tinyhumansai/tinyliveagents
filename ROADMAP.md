# Roadmap

`tinyliveagents` standardizes live voice agent APIs. It stays a library:
providers in, one session vocabulary out.

## Shipped (0.1)

- The standard session: `LiveConfig`, `ClientCommand`, `LiveEvent`,
  `LiveProvider`, `LiveSession`, one `Error` enum.
- Gemini Live, direct (API key) and relayed (pre-minted ticket URL), with
  setup / ticket builders and Gemini schema cleaning.
- ElevenLabs Agents over a signed URL or agent id + key, with client tools.
- Sarvam AI as a cascade (`saaras:v3-realtime` → `sarvam-105b-conversations`
  with tools → `bulbul:v3`), with barge-in and per-turn TTS sockets.
- PCM16 resampling helpers.
- Offline mock-server tests for every provider; live tests and examples in a
  separate crate.

## Next

- OpenAI Realtime and Azure OpenAI Realtime.
- Gemini session resumption helpers (reconnect on `GoAway` with the last
  `ResumptionHandle`).
- Sarvam latency: overlap the follow-up completion after a tool call with the
  tool preface audio, and expose `stream_type`.
- Opus / μ-law framing for telephony hosts.

## Out of scope

- Tool execution, approvals, credential storage, ticket minting, transcript
  persistence: these belong to the host or the agent harness.
- Audio capture and playback devices.

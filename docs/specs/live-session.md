# Spec: the standard live session

Status: accepted (0.1).

## Purpose

A host (OpenHuman, a CLI, a telephony bridge) should drive any live voice
provider with one loop: send audio and tool results, receive audio, transcripts
and tool calls. Provider differences are the library's problem.

## Contract

1. `LiveProvider::connect(LiveConfig)` returns a `LiveSession` or a typed
   `Error`. Configuration the provider cannot serve fails here
   (`InvalidConfig`), not mid-session.
2. The first event is always `Ready(SessionInfo)`, carrying the input and
   output audio formats. The last event is always `Closed(CloseReason)`,
   exactly once. Nothing follows `Closed`.
3. Audio is PCM16 little-endian mono both ways. The host picks the input rate
   (validated against `Capabilities::input_sample_rates`); the provider picks
   the output rate.
4. Transcripts carry the utterance *so far*. A host replaces partial text until
   an event arrives with `is_final: true`, which closes that utterance. Final
   transcripts are what a host persists.
5. Every `ToolCall` is answered by the host with a `ToolResult` carrying the
   same `call_id` and `name`, unless a `ToolCallCancelled` naming it arrives
   first. The library never runs a tool.
6. `Interrupted` means the user barged in: the host drops queued playback.
7. A non-fatal `Error` leaves the session up. A fatal one is followed by
   `Closed(Error(..))`.
8. Dropping the event stream stops the provider task and closes its sockets.
9. Debug output never contains credentials, tickets or signed URLs.

## Provider mapping

| Standard | Gemini Live | ElevenLabs Agents | Sarvam cascade |
| --- | --- | --- | --- |
| `Ready` | `setupComplete` (direct); socket open (relay) | `conversation_initiation_metadata` | STT socket open |
| `Audio` in | `realtimeInput.audio` | `user_audio_chunk` (resampled to the agent's rate) | STT `audio_input` |
| `Audio` out | `serverContent.modelTurn.parts[].inlineData` | `audio.audio_event` | TTS `audio` (`linear16`) |
| `Text` | `clientContent` turn | `user_message` | a user turn |
| `InputTranscript` | accumulated `inputTranscription`; final when the model answers | `tentative_user_transcript` / `user_transcript` | `transcript.partial` / `transcript.final` |
| `OutputTranscript` | accumulated `outputTranscription` (or text parts); final at turn end or interruption | `agent_response` (partial; final once corrected or when the next user or agent turn starts), `agent_response_correction` (final) | streamed completion text; final at the end of each completion |
| `ToolCall` | `toolCall.functionCalls[]` | `client_tool_call` | completion `tool_calls` |
| `ToolResult` | `toolResponse.functionResponses[]` (object response) | `client_tool_result` (string result) | a `tool` message, then a follow-up completion |
| `ToolCallCancelled` | `toolCallCancellation` | — | barge-in during a tool wait |
| `Interrupted` | `serverContent.interrupted` | `interruption` | `vad.speech_start` or a new utterance / typed message while a turn runs, or `ClientCommand::Interrupt` |
| `TurnComplete` | `turnComplete` (+ `usageMetadata`) | — (no such frame) | after the reply finished speaking |
| `ResumptionHandle` | `sessionResumptionUpdate` | — | — |
| `GoAway` | `goAway.timeLeft` | — | — |

Close-code mapping: 4401 → `Unauthorized`, 4402 → `InsufficientCredits`,
4408 → `Timeout` (TinyHumans relay); Gemini 1007 → `InvalidConfig`, 1008 with
an API-key reason → `Unauthorized`; Sarvam STT 1003 → `Unauthorized`, 1008 →
`Timeout`, 4000 → `InvalidConfig`; HTTP 401/403, 402 and 429 on the upgrade →
`Unauthorized`, `InsufficientCredits`, `RateLimited`.

## Sarvam cascade design

Sarvam has no speech-to-speech endpoint, so the cascade keeps the conversation
history itself and runs one *turn* task per finished user utterance:

1. the user message is appended to history by the session task;
2. the turn opens a TTS socket while the chat completion starts (hiding the
   connect latency), streams reply text into the sentence chunker and on to
   TTS, and forwards audio as it arrives;
3. if the completion ends in `tool_calls`, the turn commits the assistant
   message, emits `ToolCall`s, waits for every `ToolResult`, commits the tool
   messages and asks again (at most eight rounds);
4. otherwise it waits for TTS to finish (`final` event) and emits
   `TurnComplete`.

Barge-in (speech, a new utterance or a typed message while a turn runs)
aborts the turn task. A reply is committed to history only once it has been
fully spoken, so an interrupted reply is not kept. An assistant message with
tool calls is committed before the tools run, since they may act; calls left
unanswered by an interruption are closed with `Error: cancelled`. A completion
that fails or ends without `[DONE]` / a finish reason is neither committed nor
acted on.

## Non-goals

Tool execution, approvals, credential storage, ticket minting and transcript
persistence. See the crate documentation.

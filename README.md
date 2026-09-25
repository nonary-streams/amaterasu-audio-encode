# Audio encoder

`amaterasu.audio-encode/1` provides genuine Opus encoding through a borrowed
native audio interface. Startup settings default to `{"enabled":false}`.
Disabled startup creates no codec sessions and touches no audio devices. The
provider is independent of WASAPI: a trusted native consumer supplies PCM.

## Codec and build dependency

The module pins `opusic-sys =0.7.5` with only its `bundled` feature, which builds
and statically links the vendored reference libopus 1.6.1. Bundling bypasses
system-library lookup. There is no ambient DLL loading, handcrafted codec, or
runtime network download. The dependency supplies pregenerated Rust bindings;
`bindgen`, DRED, and OSCE features are not enabled. Its published `opus.patch`
contains build-runtime configuration changes and an OSCE definition change
(the latter is inactive here). libopus remains subject to its upstream license.

The added build dependency is `cmake` (using the existing `cc` dependency).
Builds need a C compiler and CMake. The verified macOS tool installation was:

```sh
python3 -m venv /private/tmp/amaterasu-cross/build-tools
/private/tmp/amaterasu-cross/build-tools/bin/python -m pip install \
  'cmake==3.31.10' 'ninja==1.13.0'
export PATH=/private/tmp/amaterasu-cross/build-tools/bin:$PATH
```

The normal Cargo lockfile fixes crate checksums. These build tools were fetched
as official PyPI distributions in a temporary environment, outside the repository.
See [cross-compilation instructions](../../docs/cross-compilation.md) for genuine
Linux GNU and Microsoft SDK/MSVC cross tools. Cross-compilation checks do not
qualify target execution, final linkage, device capture, or network streaming.

Primary references: [opusic-sys](https://github.com/DoumanAsh/opusic-sys),
[reference Opus encoder API](https://www.opus-codec.org/docs/opus_api-1.3.1/group__opus__encoder.html).

## Control contract

All fields are strict; unknown fields fail. Every stream and operation ID is
scoped to the runtime's authenticated immediate caller. Supplying a stream ID
never establishes ownership. The native binding likewise uses the principal
provided by the runtime, not a caller-supplied payload identity.

| Action | Fields | Result |
| --- | --- | --- |
| `enumerate` | none | `ok`, `enabled`, `encoders`; empty when disabled |
| `open` | `channels` (1 or 2, default 2), `packet_ms` (5, 10, 20; default 10), `bitrate` (6000–256000 bps, default 128000), optional `operation_id` | `ok`, `stream`, `codec:"opus"`, `sample_rate:48000`, `channels`, `packet_ms`, `bitrate`, `lookahead_frames`, `constant_bitrate:true`, `packet_bytes` |
| `lookup` | `operation_id` | Same metadata as open, or `stream_not_found` |
| `update` | `stream`, `bitrate` (6000–256000 bps) | `ok`, `stream`, current `bitrate` and `packet_bytes` |
| `close` | `stream` | `{"ok":true}`, or `stream_not_found` |
| `status` | optional `stream` | `ok`, `enabled`, caller-owned `streams` metadata and health |

Errors are `{"ok":false,"code":"..."}` without PCM or backend diagnostics.
Disabled open returns `disabled`. Unsupported settings report specific format
errors. Unknown/foreign IDs are indistinguishable; filtered status returns an
empty list. Stream IDs are nonzero OS-random 64-bit integers. Consumers must
preserve their integer precision (avoid converting them through JavaScript
floating-point numbers).

There are at most four active sessions, two per caller, 128 operation records,
and 1024 issued stream IDs per module lifetime. Operation IDs are nonempty,
control-character-free strings of at most 128 bytes. A matching retry returns
the existing stream; changed parameters return `operation_conflict`. Closed
operation records remain tombstones, so a retry cannot recreate an encoder.
`lookup` on a closed stream returns `stream_not_found`; open retry returns
`stream_closed`. Issued IDs are retained to prevent reuse within the module
lifetime. Limits return errors rather than evicting ownership records.

Updates require stream ownership; closed or foreign streams return
`stream_not_found`. Invalid bitrates leave the stream unchanged. An unchanged
bitrate is idempotent. A changed bitrate returns `stream_busy` while any packet
or partial PCM remains; consumers must reach an empty packet boundary. Drain
and failure forbid updates. Status and lookup report the current bitrate and
packet size. Retrying the original open operation does not undo an update.

A cancelled request is checked before allocation. If completing a newly opened
stream fails, that allocation is closed. Sessions and retry records are
in-memory, not durable. Close discards buffered samples/packets; consumers that
need the tail must drain and receive EOS before closing.

## Native stream behavior

`AudioEncoderApi` has `submit`, `receive`, and `drain`. It accepts 48 kHz mono
or stereo PCM only, with stereo channels ordered left/right. Input may switch
between interleaved little-endian F32 and I16 chunks, each bounded to 100 ms by
`AudioFrame`. It performs no resampling, channel remapping, or surround encoding.
I16 maps by division by 32768. Finite F32 values are clipped to [-1, 1]; NaN and
infinity are rejected before acceptance. `AUDIO_SILENT` supplies zeros regardless
of sample bytes. Bytewise conversion supports unaligned input allocations.

The restricted-lowdelay libopus mode uses constant bitrate, producing fixed
240/480/960-sample packets with a constant encoded byte length. `packet_bytes`
reports that length before transport encryption. Startup measures the actual codec's
bitrate rounding with silence, then resets it before accepting real input. Libopus
generates valid Opus padding; the transport must never append arbitrary bytes to
equalize packet sizes. Variable-bitrate output is not exposed in this profile.
A stream retains fewer than one packet of partial
PCM between submissions and at most 24 output packets, each capped at 4000
bytes. At most one 100 ms chunk is copied while submitting. Every successful
submit accepts all samples. `MEDIA_BUSY` accepts nothing and leaves timing,
codec state, and output unchanged. Backend errors fail the stream visibly;
close is then required. Calls sharing a busy controller, including reentrant
sink callbacks, return `MEDIA_BUSY` rather than waiting on its mutex.

Input sequence numbers must increase by exactly one, and input timestamps must
be nondecreasing. Finite monotonic QPC/scheduler jitter and gradual clock drift
are accepted, including `AUDIO_TIMESTAMP_ESTIMATED` input. Packet timestamps
remain on the first timestamp plus cumulative 48 kHz samples, rather than
following each noisy capture timestamp. Status exposes `last_input_timestamp_us`
and `input_clock_drift_us` (latest capture timestamp minus the corresponding
sample-clock timestamp, saturated to signed 64-bit range). These fields change
only when input is accepted; they are observations, not clock correction.

Sequence gaps/replays or timestamp regressions return `STATUS_INVALID_ARGUMENT`
without acceptance. Callers must mark actual sample discontinuities or clock
resets with `AUDIO_DISCONTINUITY`. An explicit discontinuity resets libopus,
discards only the old partial PCM (visible in `discarded_frames`), and rebases
the clock to the new timestamp. Already encoded output remains queued. The
first newly encoded packet carries `AUDIO_PACKET_DISCONTINUITY`; consumers must
reset their decoder at that packet. Output sequences increase across resets.
An accepted bitrate update likewise starts a new codec epoch and marks its
first packet as a discontinuity. The old codec delay is not flushed at this
boundary; a receiver resets its decoder and accounts for the new lookahead.
The stream owner must handle long-term audio/video clock synchronization,
including resampling when needed; this encoder does not correct capture drift.

Packet timestamps follow the input epoch and encoded sample clock. Decoders
have `lookahead_frames` of codec delay, reported at open/status, and consumers
must account for it when aligning decoded audio. Drain appends enough zeros to
flush that delay and fill the final fixed packet; `padded_frames` reports the
added silence. This may produce a tail packet even when there is no partial
input. An entirely empty stream drains directly to EOS. EOS has zero frames and
bytes; it follows all encoded packets. Drain can return BUSY until queue space
is available; it changes no state in that case. Successful drain stops input,
and repeated drain calls are harmless. EOS is delivered once after acceptance;
subsequent receive and submit return `MEDIA_CLOSED`.

Receive invokes at most one callback. A rejected packet, including EOS, remains
unchanged for retry. The sink may borrow bytes only during its callback; submit
copies input before returning. Native callers must provide valid pointers and
must not unwind across C callbacks. Factory/binding allocation, libopus state,
and callback boundaries carry explicit Rust safety comments. Module shutdown
closes every codec. Consumers must release bindings before provider unload via
signed dependency ordering. Native modules are trusted code; grants do not
sandbox arbitrary native code.

## Verification and remaining scope

Tests decode real encoder output with an independently allocated reference
libopus decoder, measuring tone reconstruction for mono/stereo, F32/I16, and
all three packet durations. Tests also cover PCM clipping/byte order, timestamp
bounds, queue retry, partial buffering, drain padding, rejected EOS, ownership,
bitrate update boundaries, reentrant callbacks, close/drop, and cancellation
before and after allocation.
The signed native-host fixture verifies dynamic module binding and real PCM to
Opus packet delivery, including exact rejected-packet retry and one EOS.

This module supplies a baseline codec provider. Capture-to-encoder coordination,
including invoking live updates from the current stream-session consumer,
resampling, surround/multistream mapping, RTP/FEC/encryption, network delivery,
Moonlight audio negotiation, and end-to-end playback are separate requirements.
No working remote audio stream or production Moonlight qualification is claimed.

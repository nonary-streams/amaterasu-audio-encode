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
[reference Opus encoder API](https://www.opus-codec.org/docs/opus_api-1.3.1/group__opus__encoder.html),
[libopus multistream API](https://www.opus-codec.org/docs/opus_api-1.6/opus__multistream_8h.html),
and [RFC 7845 channel mapping](https://www.rfc-editor.org/rfc/rfc7845.html#section-5.1.1.2).

## Control contract

All fields are strict; unknown fields fail. Every stream and operation ID is
scoped to the runtime's authenticated immediate caller. Supplying a stream ID
never establishes ownership. The native binding likewise uses the principal
provided by the runtime, not a caller-supplied payload identity.

| Action | Fields | Result |
| --- | --- | --- |
| `enumerate` | none | `ok`, `enabled`, `encoders`; empty when disabled |
| `open` | `channels` (1, 2, 6, or 8; default 2), `packet_ms` (5, 10, 20; default 10), `bitrate` within the channel range below (default 128000), optional `operation_id` | `ok`, `stream`, `codec:"opus"`, `sample_rate:48000`, `channels`, `streams`, `coupled_streams`, `mapping_family`, `channel_mapping`, `wave_channel_mapping`, `packet_ms`, `bitrate`, `lookahead_frames`, `constant_bitrate:true`, `packet_bytes` |
| `lookup` | `operation_id` | Same metadata as open, or `stream_not_found` |
| `update` | `stream`, `bitrate` within the channel range below | `ok`, `stream`, current `bitrate` and `packet_bytes` |
| `recover` | `stream` | Rebuild a failed stream's codec; `ok`, unchanged `stream`, and `packet_bytes` |
| `close` | `stream` | `{"ok":true}` for the owner, including a retry after release; `stream_not_found` for unknown or foreign IDs |
| `status` | optional `stream` | `ok`, `enabled`, caller-owned `streams` metadata and health |

Errors are `{"ok":false,"code":"..."}` without PCM or backend diagnostics.

| Channels | PCM channel order | Streams | Coupled | Bitrate range (bps) |
| --- | --- | ---: | ---: | ---: |
| 1 | mono | 1 | 0 | 6000–512000 |
| 2 | front left, front right | 1 | 1 | 6000–512000 |
| 6 | front left, front right, front center, LFE, back left, back right | 4 | 2 | 256000–1536000 |
| 8 | the six channels above, then side left, side right | 5 | 3 | 450000–2048000 (512000 minimum at 5 ms) |

For 5.1 and 7.1, callers must supply a bitrate explicitly; the omitted
128000 bps default applies only to mono and stereo.

Input PCM retains the WAVE speaker order in the table. For 5.1/7.1, the
encoder reorders it to RFC 7845 family-1 Vorbis order before native Opus
surround encoding: FL, FC, FR, BL, BR, LFE for 5.1; FL, FC, FR, SL, SR, BL,
BR, LFE for 7.1. Native surround encoding couples the front and surround
left/right pairs and gives LFE its own low-frequency stream. `mapping_family`
is 1 for surround and 0 for mono/stereo. `channel_mapping` is the canonical
Vorbis-order decoder mapping (`[0,4,1,2,3,5]` for 5.1 and
`[0,6,1,2,3,4,5,7]` for 7.1). `wave_channel_mapping` is a decoder mapping
that instead produces the WAVE order above (`[0,1,4,5,2,3]` and
`[0,1,6,7,4,5,2,3]`). The native packet ABI carries the channel count but
not these mappings; a decoder must retain the open/status metadata. `enumerate`
lists each channel layout and bitrate range, including `bitrate_min_5ms`.

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

Recovery requires a failed, caller-owned stream. Packets encoded before the
failure remain available through `receive`, including retry after a sink
rejection; an empty failed queue reports `MEDIA_BACKEND_ERROR` rather than EOS.
Recovery constructs a replacement codec before changing stream state, then
retains already encoded audio packets, removes any failed-drain EOS marker, and
discards partial PCM. Synthetic silence from a failed drain is removed before
recovery and is not counted as discarded capture input. Recovery clears failure
and input timing and accepts new input from any sequence and timestamp. Output
sequence numbers continue;
the first new packet has `AUDIO_PACKET_DISCONTINUITY` if any earlier packet was
encoded. The receiver must reset its decoder. Recovery of a healthy stream
returns `stream_not_failed`; a codec initialization error leaves the failed
stream available for inspection or close.
An update after recovery preserves the pending decoder reset marker.

A cancelled request is checked before allocation. If completing a newly opened
stream fails, that unpublished allocation and its operation record are removed,
so a retry can open it and repeated cancellations cannot exhaust operation IDs
or issued stream IDs. Explicit close still retains its replay tombstone.
Sessions and retry records are in-memory, not durable. Close discards buffered
samples/packets; consumers that need the tail must drain and receive EOS before
closing. A close retry after a lost response acknowledges the owner's already
released stream without recreating it. Other callers still receive
`stream_not_found`; native submit, receive, and drain remain closed.

## Native stream behavior

`AudioEncoderApi` has `submit`, `receive`, and `drain`. It accepts 48 kHz mono,
stereo, 5.1, or 7.1 PCM in the channel order above. Input may switch
between interleaved little-endian F32 and I16 chunks, each bounded to 100 ms by
`AudioFrame`. It performs no resampling or channel remapping.
I16 maps by division by 32768. Finite F32 values are clipped to [-1, 1]; NaN and
infinity are rejected before acceptance. `AUDIO_SILENT` supplies zeros regardless
of sample bytes. Bytewise conversion supports unaligned input allocations.

The restricted-lowdelay libopus mode uses constant bitrate, producing fixed
240/480/960-sample packets with a constant encoded byte length. `packet_bytes`
reports that length before transport encryption. Startup measures the actual codec's
bitrate rounding with silence, then resets it before accepting real input. Libopus
generates valid Opus padding; the transport must never append arbitrary bytes to
equalize packet sizes. Variable-bitrate output is not exposed in this profile.
Mono/stereo use a single Opus encoder; 5.1/7.1 use the native Opus family-1
surround encoder. The largest supported 7.1 profile produces 5120-byte 20 ms packets,
within the encoded-audio ABI's 64 KiB limit. Transport consumers must support
that size before enabling surround streaming.
A stream retains fewer than one packet of partial
PCM between submissions and at most 24 output packets, each capped at 16000
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
An accepted bitrate update changes the live Opus encoder without resetting its
history. Subsequent packets remain decoder-continuous and carry no discontinuity
marker. An update after recovery preserves the recovery reset marker.
The stream owner must handle long-term audio/video clock synchronization,
including resampling when needed; this encoder does not correct capture drift.

Packet timestamps follow the input epoch and encoded sample clock. Decoders
have `lookahead_frames` of codec delay, reported at open/status, and consumers
must account for it when aligning decoded audio. Drain appends enough zeros to
flush that delay and fill the final fixed packet; `padded_frames` accumulates the
silence in successfully encoded flush packets, including packets retained after
a later flush failure. This may produce a tail packet even when there is no partial
input. An entirely empty stream drains directly to EOS. EOS has zero frames and
bytes; it follows all encoded packets. Drain can return BUSY until queue space
is available; it changes no state in that case. Successful drain stops input,
and repeated drain calls are harmless. EOS is delivered once after acceptance;
subsequent receive and submit return `MEDIA_CLOSED`.

Receive invokes at most one callback. A rejected packet, including EOS, remains
unchanged for retry. A failed stream still delivers its previously encoded
packets in order; after they are consumed it reports `MEDIA_BACKEND_ERROR`.
The sink may borrow bytes only during its callback; submit
copies input before returning. Native callers must provide valid pointers and
must not unwind across C callbacks. Factory/binding allocation, libopus state,
and callback boundaries carry explicit Rust safety comments. Module shutdown
closes every codec. Consumers must release bindings before provider unload via
signed dependency ordering. Native modules are trusted code; grants do not
sandbox arbitrary native code.

## Verification and remaining scope

Tests decode real encoder output with independently allocated reference
libopus decoders, measuring tone reconstruction for mono/stereo, F32/I16, and
all three packet durations, plus multistream decoding for 5.1/7.1 at base and
high bitrates. Tests also cover PCM clipping/byte order, timestamp
bounds, queue retry, partial buffering, drain padding, rejected EOS, ownership,
bitrate update boundaries, reentrant callbacks, close/drop, and cancellation
before and after allocation.
The signed native-host fixture verifies dynamic module binding and real PCM to
Opus packet delivery, including exact rejected-packet retry and one EOS.

This module supplies a baseline codec provider. Capture-to-encoder coordination,
including invoking live updates from the current stream-session consumer,
resampling, surround decoder negotiation, RTP/FEC/encryption, network delivery,
Moonlight audio negotiation, and end-to-end playback are separate requirements.
No working remote audio stream or production Moonlight qualification is claimed.

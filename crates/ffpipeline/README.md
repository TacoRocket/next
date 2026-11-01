# FFmpeg pipeline builder

The builder resolves media, filters and hardware surfaces into one FFmpeg command.

## Credit squeeze

`src/credit_squeeze.rs` implements a timestamp-preserving split/composite/join,
adapted from Tunarr's `CreditSqueezeFilter`. The split precedes ordinary geometry:
the lead-in retains the existing geometry, shader and subtitle filter order. Only the credits branch
moves the channel shader after subtitle burn and composition, so the still
background receives that treatment too. Graphics remain outside both branches. The
program supplies the compositor's frame clock and EOF; its audio, output duration
and timestamp offset are unchanged. The still is decoded/uploaded once and looped.

This is an SDR, 8-bit VAAPI path. `concat=preserve_ts`, `overlay_vaapi`, libplacebo
and Vulkan/VAAPI sharing are required. Unsupported hardware, HDR and unreadable
backgrounds leave the ordinary full-screen pipeline in control and record a
`credit_squeeze_unavailable` degradation. Failures after FFmpeg launches retain
the worker's existing failure handling; the worker does not retry without squeeze.
The join needs one colour representation. The squeeze-enabled lead-in explicitly
converts SDR to limited-range BT.709, matching the compositor, and declares the
converted metadata before concat. This is a real VAAPI colour conversion, not a
retag of unknown or BT.601 pixels. Ordinary channels remain unchanged. Missing
source colour tags still depend on FFmpeg/driver interpretation; command/pixel
identity with an ordinary next transcode is not promised for that conversion.

The item-relative marker has the worker's elapsed playout time subtracted once;
joining after the marker starts squeezed. Missing settings leave ordinary items
unchanged. The scheduler owns marker selection and the original item end.

Credit squeeze is an optional fork field on playout `0.0.7`. The scheduler must use a
fork worker release supporting this field; older workers may ignore it and play
full-screen. The scheduler sets the per-item field, for example:

```json
"credit_squeeze": { "background_image": "/artwork/background.png", "start_ms": 60000 }
```

`start_ms` is relative to that playout item's start, not the original media file.
Use zero for an item already in its credits. This adds no automatic credit detection
or schedule shortening. Existing playout versions remain readable.

## VAAPI overlays

VAAPI overlays use RGBA secondary frames. BGRA is reordered in software before
upload because narrow BGRA surfaces were observed to corrupt rows during a
VAAPI upload/download round trip. `accel/vaapi.rs` selects this path only when
RGBA VPP support is reported; otherwise the existing software overlay remains
in use. The conversion preserves alpha, requested dimensions and placement.

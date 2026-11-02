# FFmpeg pipeline builder

The builder resolves media, filters and hardware surfaces into one FFmpeg command.

## Credit squeeze

`src/credit_squeeze.rs` implements a timestamp-preserving split/composite/join,
adapted from Tunarr's `CreditSqueezeFilter`. The split precedes ordinary geometry:
the lead-in retains the existing geometry, shader and subtitle filter order. Only the credits branch
moves the channel shader after subtitle burn and composition, so the still
background receives that treatment too. Graphics remain outside both branches. The
program supplies the compositor's frame clock and EOF; output duration and timestamp
offset are unchanged. Still mode preserves program audio and decodes/uploads the
image once before looping it.

This is an SDR, 8-bit VAAPI path. The bundled FFmpeg provides `concat=preserve_ts`;
`overlay_vaapi`, libplacebo and Vulkan/VAAPI sharing are required. Unsupported hardware, HDR and unreadable
backgrounds leave the ordinary full-screen pipeline in control and record a
`credit_squeeze_unavailable` degradation. Failures after FFmpeg launches retain
the worker's existing failure handling; the worker does not retry without squeeze.
The join needs one colour representation. The squeeze-enabled lead-in explicitly
converts SDR to limited-range BT.709, matching the compositor, and declares the
converted metadata before concat. This is a real VAAPI colour conversion, not a
retag of unknown or BT.601 pixels. Ordinary channels remain unchanged. Missing
source colour tags still depend on FFmpeg/driver interpretation; command/pixel
identity with an ordinary next transcode is not promised for that conversion.

At the marker, the program moves and shrinks over one second with cosine easing,
revealing the background. The final position is 5% from the left and 22.5% from
the top, at 55% of the output width and height. Graphics stay outside this move.
The compositor evaluates the move against video timestamps, not wall-clock time.
The item-relative marker has the worker's elapsed playout time subtracted once;
joining at or after the marker starts squeezed without replaying the entrance.
Missing settings leave ordinary items
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

### Moving advertisements

The optional fork extension `advertisement` replaces the still with one moving ad.
The program moves to the bottom-left over the same one-second cosine entrance,
ending at 25% of canvas width with 3% left and bottom margins. The inset preserves
program aspect ratio; its credits branch uses fit scaling without canvas padding
or cropping. The ad fits the canvas with black bars when its aspect differs;
`normalize_sar=1` also preserves anamorphic ad proportions. Deinterlacing, frame-rate conversion,
rotation, subtitles and the existing post-composition shader still apply.
Ads decode in software before upload; 4K ads can add substantial CPU work on small
servers, so 1080p or smaller ad pools keep that cost lower.

```json
"credit_squeeze": {
  "background_image": "/artwork/background.png",
  "start_ms": 60000,
  "advertisement": {
    "source": {
      "source_type": "local",
      "path": "/ads/first.mp4",
      "in_point_ms": 0,
      "out_point_ms": 30000
    },
    "elapsed_ms": 0
  }
}
```

The scheduler chooses the ad sequence and emits one ad per playout item. It ends
each item at the next ad boundary or the original program end, whichever comes
first; the final ad is cut immediately at that end. `source.in_point_ms` already
includes any ad time elapsed before the item was emitted. The worker adds only
playout time elapsed after this item's marker. `elapsed_ms` records credits time
already elapsed at item start, independently of the ad's in-point, so a later ad
or late join continues the entrance instead of restarting it. The still path and
its existing zero-marker behavior are unchanged.

Audio uses a trim/concatenate splice: original audio before the marker and ad
audio after it, normalized to a shared channel layout and 48 kHz before existing
output processing. An ad without audio produces silence. Audio copy is disabled
for moving-ad requests because the splice requires decoding. The moving ad is
not looped; source out-point limits its input. Program duration and EOF remain
authoritative.

Eligibility is shared by picture and sound. An unreadable local ad, probe failure,
missing video, exhausted clip, HDR ad or unsupported squeeze hardware keeps the
original picture and audio with `credit_squeeze_unavailable`. Probe hints retain
the existing trust contract: remote availability or invalid hinted metadata may
fail only when FFmpeg opens the source, and post-launch faults use existing worker
failure handling. The worker does not select another ad or switch back at runtime.

This additive extension keeps the upstream playout `0.0.7` URI and requires a fork
worker with moving-ad support. `background_image` remains required for contract
compatibility but is ignored when `advertisement` is present. Integrators emitting
the extension must pin a supporting worker; older workers may ignore it. No legacy
scheduler change is required unless it adopts the optional extension.

## VAAPI overlays

VAAPI overlays use RGBA secondary frames. BGRA is reordered in software before
upload because narrow BGRA surfaces were observed to corrupt rows during a
VAAPI upload/download round trip. `accel/vaapi.rs` selects this path only when
RGBA VPP support is reported; otherwise the existing software overlay remains
in use. The conversion preserves alpha, requested dimensions and placement.

## VAAPI scaling and CRT sampling

`accel/vaapi.rs` resolves the optional fork channel field
`normalization.video.filters.scale_vaapi.mode` into the video and bitmap-subtitle
VAAPI scalers. For example, `{"scale_vaapi":{"mode":"default"}}` in
`normalization.video.filters` asks the driver to choose its default scaler.
Omitting the mode preserves the existing FFmpeg command. Software fallback,
format-only conversions and other hardware backends are unchanged. Custom shader
pre-scaling retains its separate `custom_shader_options.input_scale_mode` setting.
This additive field keeps channel URI 0.1.3 and requires a fork worker containing
this extension; integrators must pin a supporting release before emitting it.

Native NV12 shader output is selected for positive-even active picture dimensions
when requested and the runtime reports `libplacebo`'s `nv12_output` option.
Padding follows the shader, so a picture does not have to fill the channel frame.
`point_chroma` uses point sampling on this native route, including pictures with
black bars; it can alias fine color detail compared with Mitchell filtering.
Without the patched runtime, the existing filtered conversion remains. With
native output disabled, the separate direct-handoff option retains filtered color.

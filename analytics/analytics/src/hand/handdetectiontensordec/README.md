# Hand Detection Tensor Decoder

The handdetectiontensordec GStreamer element decodes hand-related tensors from inference output and attaches oriented object-detection analytics metadata to video buffers.

## Overview

This element consumes tensor metadata and produces GstAnalyticsRelationMeta entries suitable for visualization with objectdetectionoverlay.

Supported input tensor styles:

- Palm detections (palm-detection-out):
  - shape per detection: [score, cx, cy, size, kp0_x, kp0_y, kp2_x, kp2_y]
  - includes decoder-side filtering and NMS
  - NMS is applied on palm decoder candidates before conversion to final hand metadata

## Requirements

- Built with v1_30 feature support.

## Properties

- confidence-threshold (float, 0.0..1.0, default 0.30)
  - The default is the recommended starting value for this model.
  - Minimum confidence to keep a hand candidate.
- max-hands (uint, 1..8, default 2)
  - Maximum number of hands reported per frame.
- nms-iou-threshold (float, 0.0..1.0, default 0.08)
  - The default is the recommended starting value for this model.
  - IoU threshold used by non-maximum suppression on palm detections.
  - IoU computation delegates to gst_analytics_image_util_iou_float() from libanalytics.

## Basic Pipeline (Palm Detection)

```bash
gst-launch-1.0 \
  v4l2src device=/dev/video0 \
  ! videoconvertscale add-borders=true \
  ! onnxinference execution-provider=cpu model-file=/path/to/palm_detection_full_inf_post_192x192.onnx \
  ! handdetectiontensordec \
  ! fakesink
```

## Visualization Pipeline

```bash
gst-launch-1.0 \
  v4l2src device=/dev/video0 \
  ! videoconvertscale add-borders=true \
  ! onnxinference execution-provider=cpu model-file=/path/to/palm_detection_full_inf_post_192x192.onnx \
  ! handdetectiontensordec confidence-threshold=0.30 nms-iou-threshold=0.08 max-hands=1 \
  ! objectdetectionoverlay draw-labels=false object-detection-outline-color=0x00FF00FF \
  ! videoconvert \
  ! autovideosink
```

Suggested tuning window for this model:
confidence-threshold=0.28..0.33 and nms-iou-threshold=0.07..0.09.

## Notes on ModelInfo Mapping

`handdetectiontensordec` attempts to decode a tensor with tensor-id `palm-detection-out`.

Your `.modelinfo` is what tells the inference element which model output should be
advertised downstream with that tensor-id.

- Decoder expects tensor-id: `palm-detection-out`

## Debugging

Enable element debug logs:

```bash
GST_DEBUG=handdetectiontensordec:5 gst-launch-1.0 ...
```

Common symptoms:

- No detections: check .modelinfo IDs and confidence threshold.
- Noisy duplicates: lower nms-iou-threshold (more suppression).
- Spatially wrong boxes: ensure pre-processing matches model expectations (RGB, aspect-preserving resize, correct input size).

## Related Elements

- onnxinference
- objectdetectionoverlay
- handlandmarktensordec
- yoloxtensordec

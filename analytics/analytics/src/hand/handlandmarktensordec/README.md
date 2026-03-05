# Hand Landmark Tensor Decoder

The handlandmarktensordec GStreamer element decodes hand landmark tensors and attaches analytics metadata to video buffers.

## Overview

This element consumes landmark model tensors and produces:

- GstTensorMeta: hand keypoint tensors
- GstAnalyticsRelationMeta: optional oriented hand bounding boxes

## Requirements

- Built with v1_30 feature support.

## Properties

- confidence-threshold (float, 0.0..1.0, default 0.5)
  - Minimum confidence to keep a hand.
- max-hands (uint, 1..10, default 2)
  - Maximum number of hands reported per frame.
- nms-iou-threshold (float, 0.0..1.0, default 0.2)
  - IoU threshold used for hand NMS.
- attach-bounding-box (bool, default true)
  - Whether to attach oriented bounding box metadata.

## Visualization Pipeline

```bash
gst-launch-1.0 \
  v4l2src device=/dev/video0 \
  ! videoconvert \
  ! videoscale \
  ! onnxinference execution-provider=cpu model-file=hand_landmark_sparse_Nx3x224x224.onnx \
  ! handlandmarktensordec confidence-threshold=0.5 max-hands=2 attach-bounding-box=true \
  ! objectdetectionoverlay draw-labels=false object-detection-outline-color=0xFFFF00FF \
  ! videoconvert \
  ! autovideosink
```

Set attach-bounding-box=false if you want only keypoints.

Suggested tuning window for this model:
confidence-threshold=0.45..0.60 and nms-iou-threshold=0.15..0.25.

## Notes on ModelInfo Mapping

Your .modelinfo must map ONNX outputs to the tensor IDs expected by the decoder.
The advertised tensor-group name should match the registry output group name:
hand_landmark.

- Required tensor id: hand_landmarks
- Optional tensor ids: hand_score

## Debugging

Enable element debug logs:

```bash
GST_DEBUG=handlandmarktensordec:5 gst-launch-1.0 ...
```

Common symptoms:

- No keypoints: check .modelinfo IDs and hand_landmarks tensor layout.
- Too many low-confidence hands: increase confidence-threshold.
- Missing boxes but visible keypoints: verify attach-bounding-box=true.

## Related Elements

- onnxinference
- objectdetectionoverlay
- handdetectiontensordec

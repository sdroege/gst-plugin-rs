// Copyright (C) 2024 Tomasz Andrzejak <andreiltd@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use super::CoordinateSystem;

pub const DEFAULT_OBJECT_DISTANCE_GAIN: f32 = 1.0;
pub const DEFAULT_OBJECT_COORDINATE_SYSTEM: CoordinateSystem = CoordinateSystem::LeftHanded;

/// 3d vector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Position {
    /// The positive x, y and z axes point forward, left and up, respectively.
    Cartesian(Vec3),
    /// The positive x, y and z axes point right, up and forward, respectively.
    LeftHanded(Vec3),
    /// The positive x and y axes point right and up, and the negative z axis points forward.
    RightHanded(Vec3),
}

impl Position {
    /// Converts the position to a Cartesian coordinate system.
    pub fn to_cartesian(self) -> Position {
        use Position::*;

        match self {
            LeftHanded(Vec3 { x, y, z }) => Cartesian(Vec3 { x: z, y: -x, z: y }),
            RightHanded(Vec3 { x, y, z }) => Cartesian(Vec3 { x: -z, y: -x, z: y }),
            Cartesian(_) => self,
        }
    }

    /// Converts the position to a left-handed coordinate system.
    pub fn to_left_handed(self) -> Position {
        use Position::*;

        match self {
            Cartesian(Vec3 { x, y, z }) => LeftHanded(Vec3 { x: -y, y: z, z: x }),
            RightHanded(Vec3 { x, y, z }) => LeftHanded(Vec3 { x, y, z: -z }),
            LeftHanded(_) => self,
        }
    }

    /// Converts the position to a right-handed coordinate system.
    pub fn to_right_handed(self) -> Position {
        use Position::*;

        match self {
            Cartesian(Vec3 { x, y, z }) => RightHanded(Vec3 { x: -y, y: z, z: -x }),
            LeftHanded(Vec3 { x, y, z }) => RightHanded(Vec3 { x, y, z: -z }),
            RightHanded(_) => self,
        }
    }

    /// Retrieves the underlying `Vec3` from the `Position`.
    pub fn to_vec3(self) -> Vec3 {
        use Position::*;

        match self {
            Cartesian(v) | LeftHanded(v) | RightHanded(v) => v,
        }
    }

    /// Calculates the Euclidean distance to another position.
    ///
    /// If the positions are in different coordinate systems, they are both
    /// converted to the Cartesian system before the distance is calculated.
    pub fn distance_to(self, other: Position) -> f32 {
        let self_cartesian = self.to_cartesian();
        let other_cartesian = other.to_cartesian();

        match (self_cartesian, other_cartesian) {
            (
                Position::Cartesian(Vec3 {
                    x: x1,
                    y: y1,
                    z: z1,
                }),
                Position::Cartesian(Vec3 {
                    x: x2,
                    y: y2,
                    z: z2,
                }),
            ) => {
                let dx = x2 - x1;
                let dy = y2 - y1;
                let dz = z2 - z1;

                (dx * dx + dy * dy + dz * dz).sqrt()
            }
            _ => unreachable!(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpatialObject {
    /// Position values use a left-handed Cartesian coordinate system
    pub position: Position,
    /// Object attenuation by distance
    pub distance_gain: f32,
}

impl Default for SpatialObject {
    fn default() -> Self {
        Self {
            position: Position::Cartesian(Vec3::new(1000.0, 1000.0, 1000.0)),
            distance_gain: DEFAULT_OBJECT_DISTANCE_GAIN,
        }
    }
}

impl From<gst::Structure> for SpatialObject {
    fn from(s: gst::Structure) -> Self {
        let distance_gain = s
            .get("distance-gain")
            .unwrap_or(DEFAULT_OBJECT_DISTANCE_GAIN);

        let coords_system = s
            .get("coordinate-system")
            .unwrap_or(DEFAULT_OBJECT_COORDINATE_SYSTEM);

        let v = Vec3 {
            x: s.get("x").expect("type checked upstream"),
            y: s.get("y").expect("type checked upstream"),
            z: s.get("z").expect("type checked upstream"),
        };

        let position = match coords_system {
            CoordinateSystem::Cartesian => Position::Cartesian(v),
            CoordinateSystem::LeftHanded => Position::LeftHanded(v),
            CoordinateSystem::RightHanded => Position::RightHanded(v),
        };

        SpatialObject {
            position,
            distance_gain,
        }
    }
}

impl From<SpatialObject> for gst::Structure {
    fn from(obj: SpatialObject) -> Self {
        let (coords_system, v) = match obj.position {
            Position::Cartesian(v) => (CoordinateSystem::Cartesian, v),
            Position::LeftHanded(v) => (CoordinateSystem::LeftHanded, v),
            Position::RightHanded(v) => (CoordinateSystem::RightHanded, v),
        };

        gst::Structure::builder("application/spatial-object")
            .field("x", v.x)
            .field("y", v.y)
            .field("z", v.z)
            .field("distance-gain", obj.distance_gain)
            .field("coordinate-system", coords_system)
            .build()
    }
}

impl TryFrom<gst_audio::AudioChannelPosition> for SpatialObject {
    type Error = gst::FlowError;

    fn try_from(pos: gst_audio::AudioChannelPosition) -> Result<Self, gst::FlowError> {
        use gst_audio::AudioChannelPosition::*;

        let position = match pos {
            FrontLeft => Vec3::new(-1.45, 0.0, 2.5),
            FrontRight => Vec3::new(1.45, 0.0, 2.5),
            FrontCenter | Mono => Vec3::new(0.0, 0.0, 2.5),
            Lfe1 | Lfe2 => Vec3::new(0.0, 0.0, 0.0),
            RearLeft => Vec3::new(-1.45, 0.0, -2.5),
            RearRight => Vec3::new(1.45, 0.0, -2.5),
            FrontLeftOfCenter => Vec3::new(-0.72, 0.0, 2.5),
            FrontRightOfCenter => Vec3::new(0.72, 0.0, 2.5),
            RearCenter => Vec3::new(0.0, 0.0, -2.5),
            SideLeft => Vec3::new(-2.5, 0.0, -0.44),
            SideRight => Vec3::new(2.5, 0.0, -0.44),
            TopFrontLeft => Vec3::new(-0.72, 2.5, 1.25),
            TopFrontRight => Vec3::new(0.72, 2.5, 1.25),
            TopFrontCenter => Vec3::new(0.0, 2.5, 1.25),
            TopCenter => Vec3::new(0.0, 2.5, 0.0),
            TopRearLeft => Vec3::new(-0.72, 2.5, -1.25),
            TopRearRight => Vec3::new(0.72, 2.5, -1.25),
            TopSideLeft => Vec3::new(-1.25, 2.5, -0.22),
            TopSideRight => Vec3::new(1.25, 2.5, -0.22),
            TopRearCenter => Vec3::new(0.0, 2.5, -1.25),
            BottomFrontCenter => Vec3::new(0.0, -2.5, 1.25),
            BottomFrontLeft => Vec3::new(-0.72, -2.5, 1.25),
            BottomFrontRight => Vec3::new(0.72, -2.5, 1.25),
            WideLeft => Vec3::new(-2.5, 0.0, 1.45),
            WideRight => Vec3::new(2.5, 0.0, 1.45),
            SurroundLeft => Vec3::new(-2.5, 0.0, -1.45),
            SurroundRight => Vec3::new(2.5, 0.0, -1.45),
            _ => return Err(gst::FlowError::NotSupported),
        };

        Ok(SpatialObject {
            position: Position::LeftHanded(position),
            distance_gain: DEFAULT_OBJECT_DISTANCE_GAIN,
        })
    }
}

impl From<Vec3> for hrtf::Vec3 {
    fn from(value: Vec3) -> hrtf::Vec3 {
        hrtf::Vec3 {
            x: value.x,
            y: value.y,
            z: value.z,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cartesian_to_left_handed() {
        let cartesian = Position::Cartesian(Vec3 {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        });

        match cartesian.to_left_handed() {
            Position::LeftHanded(Vec3 { x, y, z }) => {
                assert_eq!(x, -2.0);
                assert_eq!(y, 3.0);
                assert_eq!(z, 1.0);
            }
            _ => panic!("Conversion failed"),
        }
    }

    #[test]
    fn cartesian_to_right_handed() {
        let cartesian = Position::Cartesian(Vec3 {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        });

        match cartesian.to_right_handed() {
            Position::RightHanded(Vec3 { x, y, z }) => {
                assert_eq!(x, -2.0);
                assert_eq!(y, 3.0);
                assert_eq!(z, -1.0);
            }
            _ => panic!("Conversion failed"),
        }
    }

    #[test]
    fn left_handed_to_cartesian() {
        let left_handed = Position::LeftHanded(Vec3 {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        });

        match left_handed.to_cartesian() {
            Position::Cartesian(Vec3 { x, y, z }) => {
                assert_eq!(x, 3.0);
                assert_eq!(y, -1.0);
                assert_eq!(z, 2.0);
            }
            _ => panic!("Conversion failed"),
        }
    }
}

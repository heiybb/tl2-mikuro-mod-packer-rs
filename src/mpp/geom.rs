//! Minimal Ogre-compatible 3D math for the pathing pipeline (port of geom.py).
//! All f64; operation order mirrors the Python reference exactly so results
//! are bit-identical.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Vec3 { x, y, z }
    }

    pub fn cross(self, o: Vec3) -> Vec3 {
        Vec3::new(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    pub fn dot(self, o: Vec3) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn length(self) -> f64 {
        self.dot(self).sqrt()
    }

    pub fn normalized(self) -> Vec3 {
        let n = self.length();
        if n < 1e-12 {
            Vec3::new(0.0, 0.0, 0.0)
        } else {
            Vec3::new(self.x / n, self.y / n, self.z / n)
        }
    }
}

/// Row-major 4x4 affine transform, m[r][c].
#[derive(Clone, Copy, Debug)]
pub struct Mat4 {
    pub m: [[f64; 4]; 4],
}

impl Default for Mat4 {
    fn default() -> Self {
        Self::identity()
    }
}

impl Mat4 {
    pub fn identity() -> Self {
        let mut m = [[0.0; 4]; 4];
        for (r, row) in m.iter_mut().enumerate() {
            row[r] = 1.0;
        }
        Mat4 { m }
    }

    pub fn translation(v: Vec3) -> Self {
        let mut t = Self::identity();
        t.m[0][3] = v.x;
        t.m[1][3] = v.y;
        t.m[2][3] = v.z;
        t
    }

    pub fn scale(sx: f64, sy: f64, sz: f64) -> Self {
        let mut t = Self::identity();
        t.m[0][0] = sx;
        t.m[1][1] = sy;
        t.m[2][2] = sz;
        t
    }

    /// Rotation whose columns are the local X(right), Y(up), Z(forward) axes.
    pub fn from_axes(right: Vec3, up: Vec3, forward: Vec3) -> Self {
        let mut t = Self::identity();
        t.m[0][0] = right.x;
        t.m[1][0] = right.y;
        t.m[2][0] = right.z;
        t.m[0][1] = up.x;
        t.m[1][1] = up.y;
        t.m[2][1] = up.z;
        t.m[0][2] = forward.x;
        t.m[1][2] = forward.y;
        t.m[2][2] = forward.z;
        t
    }

    /// self @ other — the exact left-to-right sum order of the Python __matmul__.
    pub fn matmul(&self, o: &Mat4) -> Mat4 {
        let a = &self.m;
        let b = &o.m;
        let mut r = Mat4::identity();
        for i in 0..4 {
            for j in 0..4 {
                r.m[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j] + a[i][3] * b[3][j];
            }
        }
        r
    }

    pub fn transform_point(&self, v: Vec3) -> Vec3 {
        let m = &self.m;
        Vec3::new(
            m[0][0] * v.x + m[0][1] * v.y + m[0][2] * v.z + m[0][3],
            m[1][0] * v.x + m[1][1] * v.y + m[1][2] * v.z + m[1][3],
            m[2][0] * v.x + m[2][1] * v.y + m[2][2] * v.z + m[2][3],
        )
    }
}

/// Axis-aligned bounding box with a null state.
#[derive(Clone, Copy, Debug)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
    pub null: bool,
}

impl Default for Aabb {
    fn default() -> Self {
        Self::null()
    }
}

impl Aabb {
    pub fn null() -> Self {
        Aabb {
            min: Vec3::default(),
            max: Vec3::default(),
            null: true,
        }
    }

    pub fn new(mn: Vec3, mx: Vec3) -> Self {
        Aabb {
            min: mn,
            max: mx,
            null: false,
        }
    }

    pub fn merge_point(&mut self, p: Vec3) {
        if self.null {
            self.min = p;
            self.max = p;
            self.null = false;
        } else {
            self.min = Vec3::new(self.min.x.min(p.x), self.min.y.min(p.y), self.min.z.min(p.z));
            self.max = Vec3::new(self.max.x.max(p.x), self.max.y.max(p.y), self.max.z.max(p.z));
        }
    }

    /// The 8 corners in the Python reference order.
    pub fn corners(&self) -> [Vec3; 8] {
        let (mn, mx) = (self.min, self.max);
        [
            Vec3::new(mn.x, mn.y, mn.z),
            Vec3::new(mx.x, mn.y, mn.z),
            Vec3::new(mn.x, mx.y, mn.z),
            Vec3::new(mx.x, mx.y, mn.z),
            Vec3::new(mn.x, mn.y, mx.z),
            Vec3::new(mx.x, mn.y, mx.z),
            Vec3::new(mn.x, mx.y, mx.z),
            Vec3::new(mx.x, mx.y, mx.z),
        ]
    }
}

/// (right, up, forward) from layout FORWARD + RIGHT; UP = FORWARD x RIGHT.
pub fn orientation_axes(fwd: Vec3, right: Vec3) -> (Vec3, Vec3, Vec3) {
    let f = fwd.normalized();
    let r = right.normalized();
    let up = f.cross(r).normalized();
    (r, up, f)
}

/// Axes from a YAW (degrees) about +Y.
pub fn yaw_axes(yaw_deg: f64) -> (Vec3, Vec3, Vec3) {
    let a = yaw_deg.to_radians();
    let (sa, ca) = (a.sin(), a.cos());
    let fwd = Vec3::new(sa, 0.0, ca);
    let right = Vec3::new(ca, 0.0, -sa);
    let up = Vec3::new(0.0, 1.0, 0.0);
    (right, up, fwd)
}

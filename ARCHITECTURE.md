### Architecture

- **`zoro`**: The main crate that uses either curve implementation based on feature flags
- **`zoro_miden_client`**: Communication with the Miden node.
- **`zoro_primitives`**: Contains the `Curve` trait and a `DummyCurve` implementation
- **`zoro_curve`**: Contains the proprietary `ZoroCurve` implementation (not published, kept private)


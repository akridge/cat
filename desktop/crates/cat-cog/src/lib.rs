//! `cat-cog` — read Cloud Optimized GeoTIFFs from the public GCS bucket and
//! decode their internal tiles for display, in pure Rust (no GDAL).
//!
//! The desktop map draws each COG internal tile as a textured quad placed in
//! WebMercator, choosing the overview level that matches the screen
//! resolution, so no CPU resampling is needed at any zoom.

pub mod cog;
pub mod decode;
pub mod error;
pub mod fetch;
pub mod geo;
pub mod source;
pub mod tiff;

pub use cog::{Cog, Crs, Level};
pub use decode::{DecodeOptions, DecodedTile};
pub use error::{CogError, Result};
pub use geo::MercBox;

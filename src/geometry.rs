use std::io;

use vivid_protocol::cbor::Value;
use vivid_sdk::{Fit, RequestMetadata, SceneNode, Session, Surface};

pub const DOOM_WIDTH: u32 = 640;
pub const DOOM_HEIGHT: u32 = 400;
const FIXED_SHIFT: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalGeometry {
    pub viewport_width_px: u32,
    pub viewport_height_px: u32,
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

impl TerminalGeometry {
    pub fn from_descriptor(descriptor: &[(u64, Value)]) -> io::Result<Self> {
        if descriptor.len() != 9
            || descriptor
                .iter()
                .enumerate()
                .any(|(index, (key, _))| *key != index as u64)
        {
            return Err(invalid_data(
                "terminal target descriptor must contain exactly keys 0 through 8",
            ));
        }
        let unsigned = |key: usize| {
            descriptor[key].1.as_u64().ok_or_else(|| {
                invalid_data(format!(
                    "terminal target descriptor key {key} is not unsigned"
                ))
            })
        };
        let viewport_width_px =
            u32::try_from(unsigned(0)?).map_err(|_| invalid_data("viewport width exceeds u32"))?;
        let viewport_height_px =
            u32::try_from(unsigned(1)?).map_err(|_| invalid_data("viewport height exceeds u32"))?;
        let cols = u16::try_from(unsigned(2)?).map_err(|_| invalid_data("columns exceed u16"))?;
        let rows = u16::try_from(unsigned(3)?).map_err(|_| invalid_data("rows exceed u16"))?;
        let cell_width_px =
            u32::try_from(unsigned(4)?).map_err(|_| invalid_data("cell width exceeds u32"))?;
        let cell_height_px =
            u32::try_from(unsigned(5)?).map_err(|_| invalid_data("cell height exceeds u32"))?;
        if descriptor[6].1.as_bool().is_none()
            || viewport_width_px == 0
            || viewport_height_px == 0
            || cols == 0
            || rows == 0
            || cell_width_px == 0
            || cell_height_px == 0
            || unsigned(7)? != 3
            || unsigned(8)? == 0
        {
            return Err(invalid_data(
                "terminal target descriptor contains invalid geometry or anchor capability",
            ));
        }
        Ok(Self {
            viewport_width_px,
            viewport_height_px,
            cols,
            rows,
            cell_width_px,
            cell_height_px,
        })
    }

    pub fn layout(self, scale: bool) -> FrameLayout {
        let (columns, rows) = if scale {
            scaled_cells(self)
        } else {
            natural_cells(self)
        };
        let column = (u32::from(self.cols).saturating_sub(columns)) / 2;
        let row = (u32::from(self.rows).saturating_sub(rows)) / 2;
        FrameLayout {
            terminal: self,
            column,
            row,
            columns,
            rows,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    pub terminal: TerminalGeometry,
    pub column: u32,
    pub row: u32,
    pub columns: u32,
    pub rows: u32,
}

impl FrameLayout {
    pub fn image_rect_px(self) -> PixelRect {
        let node_x = self.column.saturating_mul(self.terminal.cell_width_px);
        let node_y = self.row.saturating_mul(self.terminal.cell_height_px);
        let node_width = self.columns.saturating_mul(self.terminal.cell_width_px);
        let node_height = self.rows.saturating_mul(self.terminal.cell_height_px);
        let width_from_height =
            u64::from(node_height).saturating_mul(u64::from(DOOM_WIDTH)) / u64::from(DOOM_HEIGHT);
        let (width, height) = if width_from_height <= u64::from(node_width) {
            (
                u32::try_from(width_from_height).unwrap_or(u32::MAX),
                node_height,
            )
        } else {
            let height = u64::from(node_width).saturating_mul(u64::from(DOOM_HEIGHT))
                / u64::from(DOOM_WIDTH);
            (node_width, u32::try_from(height).unwrap_or(u32::MAX))
        };
        PixelRect {
            x: node_x.saturating_add(node_width.saturating_sub(width) / 2),
            y: node_y.saturating_add(node_height.saturating_sub(height) / 2),
            width: width.max(1),
            height: height.max(1),
        }
    }

    pub fn mouse_delta(self, previous: (u16, u16), current: (u16, u16)) -> (i32, i32) {
        let rect = self.image_rect_px();
        let dx = i32::from(current.0).saturating_sub(i32::from(previous.0));
        let dy = i32::from(current.1).saturating_sub(i32::from(previous.1));
        let scaled_x = i64::from(dx).saturating_mul(i64::from(DOOM_WIDTH)) / i64::from(rect.width);
        let scaled_y =
            i64::from(dy).saturating_mul(i64::from(DOOM_HEIGHT)) / i64::from(rect.height);
        (
            i32::try_from(scaled_x).unwrap_or(if scaled_x < 0 { i32::MIN } else { i32::MAX }),
            i32::try_from(scaled_y).unwrap_or(if scaled_y < 0 { i32::MIN } else { i32::MAX }),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub fn scene_node(
    session: &Session,
    surface: &Surface,
    node_id: u64,
    layout: FrameLayout,
) -> io::Result<SceneNode> {
    Ok(SceneNode {
        owning_context_id: session.info().root_context_id,
        node_id,
        surface_context_id: surface.context_id(),
        surface_id: surface.id(),
        geometry: vec![
            (0, Value::Unsigned(1)),
            (1, fixed(layout.column)?),
            (2, fixed(layout.row)?),
            (3, fixed(layout.columns)?),
            (4, fixed(layout.rows)?),
            (5, Value::Unsigned(1)),
        ],
        fit: Fit::Contain,
        linear_sampling: false,
        z_index: 0,
        visible: true,
        opacity: u16::MAX,
        clip: None,
    })
}

pub fn update_scene_node(
    session: &mut Session,
    node: &mut SceneNode,
    layout: FrameLayout,
) -> io::Result<()> {
    for (key, value) in &mut node.geometry {
        match *key {
            1 => *value = fixed(layout.column)?,
            2 => *value = fixed(layout.row)?,
            3 => *value = fixed(layout.columns)?,
            4 => *value = fixed(layout.rows)?,
            _ => {}
        }
    }
    session
        .update_node(node, &RequestMetadata::default())
        .map(|_| ())
}

fn scaled_cells(geometry: TerminalGeometry) -> (u32, u32) {
    let available_width = u64::from(geometry.cols) * u64::from(geometry.cell_width_px);
    let available_height = u64::from(geometry.rows) * u64::from(geometry.cell_height_px);
    let width_from_height =
        available_height.saturating_mul(u64::from(DOOM_WIDTH)) / u64::from(DOOM_HEIGHT);
    let (width, height) = if width_from_height <= available_width {
        (width_from_height, available_height)
    } else {
        (
            available_width,
            available_width.saturating_mul(u64::from(DOOM_HEIGHT)) / u64::from(DOOM_WIDTH),
        )
    };
    (
        u32::try_from(width / u64::from(geometry.cell_width_px))
            .unwrap_or(u32::MAX)
            .clamp(1, u32::from(geometry.cols)),
        u32::try_from(height / u64::from(geometry.cell_height_px))
            .unwrap_or(u32::MAX)
            .clamp(1, u32::from(geometry.rows)),
    )
}

fn natural_cells(geometry: TerminalGeometry) -> (u32, u32) {
    (
        ceil_div(DOOM_WIDTH, geometry.cell_width_px).clamp(1, u32::from(geometry.cols)),
        ceil_div(DOOM_HEIGHT, geometry.cell_height_px).clamp(1, u32::from(geometry.rows)),
    )
}

fn ceil_div(value: u32, divisor: u32) -> u32 {
    value.saturating_add(divisor - 1) / divisor
}

fn fixed(cells: u32) -> io::Result<Value> {
    i64::from(cells)
        .checked_shl(FIXED_SHIFT)
        .map(|value| Value::Unsigned(value as u64))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cell geometry overflows"))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> TerminalGeometry {
        TerminalGeometry {
            viewport_width_px: 1000,
            viewport_height_px: 800,
            cols: 100,
            rows: 40,
            cell_width_px: 10,
            cell_height_px: 20,
        }
    }

    #[test]
    fn scaled_layout_is_centered_and_preserves_aspect() {
        let layout = geometry().layout(true);
        assert_eq!((layout.column, layout.row), (0, 4));
        assert_eq!((layout.columns, layout.rows), (100, 31));
        let rect = layout.image_rect_px();
        assert!((rect.width as f32 / rect.height as f32 - 1.6).abs() < 0.01);
    }

    #[test]
    fn natural_layout_is_centered() {
        let layout = geometry().layout(false);
        assert_eq!((layout.columns, layout.rows), (64, 20));
        assert_eq!((layout.column, layout.row), (18, 10));
    }

    #[test]
    fn pixel_mouse_delta_accounts_for_fitted_size() {
        let layout = geometry().layout(false);
        assert_eq!(layout.mouse_delta((100, 100), (110, 120)), (10, 20));
    }
}

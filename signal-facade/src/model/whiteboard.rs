use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Whiteboard drawing message sent via DataChannel
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum WhiteboardMessage {
    /// Draw a stroke with a specific tool
    #[serde(rename = "draw")]
    Draw(WhiteboardDrawData),
    /// Place text at a position
    #[serde(rename = "text")]
    Text(WhiteboardTextData),
    /// Erase specific elements by their IDs
    #[serde(rename = "erase")]
    Erase(WhiteboardEraseData),
    /// Clear all whiteboard content
    #[serde(rename = "clear")]
    Clear,
    /// Undo the last operation
    #[serde(rename = "undo")]
    Undo,
}

/// A 2D point with normalized coordinates (0.0 ~ 1.0)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WhiteboardPoint {
    pub x: f64,
    pub y: f64,
}

/// Drawing tool types
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum WhiteboardTool {
    Pen,
    Line,
    Rect,
    Circle,
    Arrow,
}

/// Data for a draw operation
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WhiteboardDrawData {
    /// Drawing tool used
    pub tool: WhiteboardTool,
    /// List of points (normalized 0.0~1.0)
    pub points: Vec<WhiteboardPoint>,
    /// CSS color string (e.g. "#ff0000")
    pub color: String,
    /// Stroke width in logical pixels
    pub width: f64,
    /// Unique element ID for undo/erase
    pub id: String,
}

/// Data for a text placement operation
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WhiteboardTextData {
    /// X position (normalized 0.0~1.0)
    pub x: f64,
    /// Y position (normalized 0.0~1.0)
    pub y: f64,
    /// Text content
    pub content: String,
    /// CSS color string
    pub color: String,
    /// Font size in logical pixels
    pub font_size: f64,
    /// Unique element ID for undo/erase
    pub id: String,
}

/// Data for erasing specific elements
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WhiteboardEraseData {
    /// IDs of elements to erase
    pub ids: Vec<String>,
}

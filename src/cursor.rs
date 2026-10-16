use winit_core::cursor::CursorIcon;

pub(crate) fn style(icon: CursorIcon) -> i32 {
    match icon {
        CursorIcon::Text | CursorIcon::VerticalText => 26,
        CursorIcon::Crosshair | CursorIcon::Cell => 13,
        CursorIcon::Grab => 18,
        CursorIcon::Grabbing => 17,
        CursorIcon::Pointer => 19,
        CursorIcon::WResize => 2,
        CursorIcon::EResize => 1,
        CursorIcon::EwResize | CursorIcon::ColResize => 5,
        CursorIcon::NResize => 4,
        CursorIcon::SResize => 3,
        CursorIcon::NsResize | CursorIcon::RowResize => 6,
        CursorIcon::NwResize | CursorIcon::SeResize | CursorIcon::NwseResize => 12,
        CursorIcon::NeResize | CursorIcon::SwResize | CursorIcon::NeswResize => 11,
        CursorIcon::NotAllowed | CursorIcon::NoDrop => 15,
        CursorIcon::Copy => 14,
        CursorIcon::Move | CursorIcon::AllScroll => 16,
        CursorIcon::Wait => 20,
        CursorIcon::Progress => 21,
        CursorIcon::Help => 23,
        CursorIcon::ZoomIn => 24,
        CursorIcon::ZoomOut => 25,
        _ => 0,
    }
}

#[derive(Debug)]
pub(crate) struct ImageCursor(pub winit_core::cursor::CursorImage);
impl winit_core::cursor::CustomCursorProvider for ImageCursor {
    fn is_animated(&self) -> bool {
        false
    }
}

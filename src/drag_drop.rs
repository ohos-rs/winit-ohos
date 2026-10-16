//! Owned drag payloads; never retain callback-scoped native pointers.
use openharmony_ability::{DragInputData, DragPhase, DragResponse};
use std::{
    collections::HashMap,
    ops::ControlFlow,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};
use winit_core::{
    data_transfer::{DataTransfer, DataTransferId, TransferType, TypeHint, TypedData},
    error::TransferError,
    event::WindowEvent,
    event_loop::{AsyncRequestSerial, DndAction},
    window::WindowId,
};

#[derive(Debug, Clone)]
struct Files(Vec<String>);
impl DataTransfer for Files {
    fn for_each_available_type<'a>(
        &'a self,
        f: &mut dyn FnMut(&'a dyn TransferType) -> ControlFlow<()>,
    ) {
        let _ = f(&TypeHint::UriList);
    }
}
impl TypedData for Files {
    fn type_(&self) -> &dyn TransferType {
        &TypeHint::UriList
    }
    fn try_read(&self) -> Option<Box<dyn std::io::BufRead>> {
        Some(Box::new(std::io::Cursor::new(
            self.0.join("\r\n").into_bytes(),
        )))
    }
    fn try_as_string(&self) -> std::io::Result<String> {
        Ok(self.0.join("\r\n"))
    }
    fn try_as_uris(&self) -> std::io::Result<Vec<String>> {
        Ok(self.0.clone())
    }
}
#[derive(Debug)]
struct Incoming {
    window: WindowId,
    files: Files,
    response: DragResponse,
    accepted: Option<openharmony_ability::DragOperation>,
    requests: Vec<AsyncRequestSerial>,
}
#[derive(Debug, Default)]
pub(crate) struct DragDrop {
    incoming: HashMap<DataTransferId, Incoming>,
    current: Option<DataTransferId>,
    pending: Vec<(WindowId, WindowEvent)>,
}
pub(crate) fn next_id() -> DataTransferId {
    static NEXT: AtomicI64 = AtomicI64::new(1);
    DataTransferId::from_raw(NEXT.fetch_add(1, Ordering::Relaxed))
}
impl DragDrop {
    pub fn event(
        &mut self,
        window: WindowId,
        drag: &DragInputData,
        offset: dpi::PhysicalPosition<i32>,
    ) -> Vec<WindowEvent> {
        let position = dpi::PhysicalPosition::new(
            f64::from(drag.window_x) - f64::from(offset.x),
            f64::from(drag.window_y) - f64::from(offset.y),
        );
        if drag.phase == DragPhase::Enter {
            let id = next_id();
            // Only one platform drag can be active. Release abandoned payloads.
            self.incoming.clear();
            self.current = Some(id);
            self.incoming.insert(
                id,
                Incoming {
                    window,
                    files: Files(drag.file_uris.clone()),
                    response: drag.response.clone(),
                    accepted: None,
                    requests: Vec::new(),
                },
            );
            return vec![WindowEvent::DragEntered {
                id,
                position: Some(position),
            }];
        }
        let Some(id) = self.current else {
            return Vec::new();
        };
        let Some(incoming) = self.incoming.get_mut(&id) else {
            return Vec::new();
        };
        incoming.response = drag.response.clone();
        if let Some(operation) = incoming.accepted {
            drag.response.accept_operation(operation);
        }
        match drag.phase {
            DragPhase::Move => vec![WindowEvent::DragPosition {
                id,
                position,
                proposed_action: incoming.accepted.map(winit_operation),
            }],
            DragPhase::Drop => {
                if incoming.accepted.is_none() {
                    self.current = None;
                    self.incoming.remove(&id);
                    return vec![WindowEvent::DragLeft { id }];
                }
                incoming.files = Files(drag.file_uris.clone());
                for serial in incoming.requests.drain(..) {
                    self.pending.push((
                        window,
                        WindowEvent::DataTransferReceived {
                            id,
                            serial,
                            value: Arc::new(incoming.files.clone()),
                        },
                    ));
                }
                vec![WindowEvent::DragDropped {
                    id,
                    proposed_action: incoming.accepted.map(winit_operation),
                }]
            }
            DragPhase::Leave => {
                self.current = None;
                self.incoming.remove(&id);
                vec![WindowEvent::DragLeft { id }]
            }
            DragPhase::Enter => unreachable!(),
        }
    }
    pub fn data(&self, id: DataTransferId) -> Result<Box<dyn DataTransfer>, TransferError> {
        self.incoming
            .get(&id)
            .map(|v| Box::new(v.files.clone()) as _)
            .ok_or(TransferError::UnknownTransfer(id))
    }
    pub fn fetch(
        &mut self,
        id: DataTransferId,
        ty: &dyn TransferType,
    ) -> Result<AsyncRequestSerial, TransferError> {
        let incoming = self
            .incoming
            .get_mut(&id)
            .ok_or(TransferError::UnknownTransfer(id))?;
        if ty.hint() != Some(TypeHint::UriList) {
            return Err(TransferError::Failed);
        }
        let serial = AsyncRequestSerial::get();
        // OHOS may disclose URIs only when dropping; preserve the original request.
        if incoming.files.0.is_empty() {
            incoming.requests.push(serial);
        } else {
            self.pending.push((
                incoming.window,
                WindowEvent::DataTransferReceived {
                    id,
                    serial,
                    value: Arc::new(incoming.files.clone()),
                },
            ));
        }
        Ok(serial)
    }
    pub fn accept(
        &mut self,
        id: DataTransferId,
        actions: &[DndAction],
    ) -> Result<(), TransferError> {
        let incoming = self
            .incoming
            .get_mut(&id)
            .ok_or(TransferError::UnknownTransfer(id))?;
        incoming.accepted = actions.iter().find_map(|action| match action {
            DndAction::Copy => Some(openharmony_ability::DragOperation::Copy),
            DndAction::Move => Some(openharmony_ability::DragOperation::Move),
            _ => None,
        });
        if let Some(operation) = incoming.accepted {
            incoming.response.accept_operation(operation);
        } else {
            incoming.response.reject();
        }
        Ok(())
    }
    pub fn drain(&mut self) -> Vec<(WindowId, WindowEvent)> {
        std::mem::take(&mut self.pending)
    }
}

/// Convert OHOS provider URIs, including bundle authorities, to accessible paths.
pub fn file_path(uri: &str) -> Option<std::path::PathBuf> {
    if !uri.starts_with("file://") {
        return None;
    }
    ohos_fileuri_binding::get_path_from_uri(uri)
        .ok()
        .map(Into::into)
}

/// Encode an accessible local path for a native file drag or clipboard record.
pub fn file_uri(path: &std::path::Path) -> Option<String> {
    ohos_fileuri_binding::get_uri_from_path(path.to_str()?).ok()
}

// Native drag actions and their callbacks belong to the Ability's UI thread.
// Keep them outside Send/Sync window handles and release before surface destruction.
thread_local! {
    // OHOS uses emulated TLS; rustc 1.95 lints the macro-generated fallback.
    #[allow(clippy::missing_const_for_thread_local)]
    static OUTGOING: std::cell::RefCell<Option<Outgoing>> = const { std::cell::RefCell::new(None) };
    #[allow(clippy::missing_const_for_thread_local)]
    static POINTER: std::cell::Cell<(WindowId, i32)> = const { std::cell::Cell::new((WindowId::from_raw(0), 0)) };
}
struct Outgoing {
    id: DataTransferId,
    source: WindowId,
    session: Option<openharmony_ability::NativeFileDrag>,
    failed: bool,
    actions: Vec<DndAction>,
}
pub(crate) fn set_pointer(window: WindowId, pointer: Option<i32>) {
    POINTER.set((
        window,
        pointer.filter(|id| (0..=9).contains(id)).unwrap_or(0),
    ));
}
pub(crate) fn parse_uris(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Start a native file drag outside the input callback. Completion reports the
/// native result and selected operation; failed/cancelled drops never become success.
pub fn start_file_drag(
    app: &openharmony_ability::OpenHarmonyApp,
    source: WindowId,
    uris: Vec<String>,
) -> Result<DataTransferId, TransferError> {
    start_file_drag_with_options(app, source, uris, vec![DndAction::Copy], None)
}

pub fn start_file_drag_with_options(
    app: &openharmony_ability::OpenHarmonyApp,
    source: WindowId,
    uris: Vec<String>,
    actions: Vec<DndAction>,
    preview: Option<openharmony_ability::DragPreview>,
) -> Result<DataTransferId, TransferError> {
    let scheduler = app.main_thread().map_err(|_| TransferError::Failed)?;
    if uris.is_empty()
        || uris.len() > 512
        || uris.iter().any(|uri| !uri.starts_with("file://"))
        || app.native_window_for(source.into_raw() as i64).is_none()
    {
        return Err(TransferError::Failed);
    }
    let id = next_id();
    OUTGOING.with_borrow_mut(|outgoing| {
        if outgoing.is_some() {
            return Err(TransferError::Failed);
        }
        *outgoing = Some(Outgoing {
            id,
            source,
            session: None,
            failed: false,
            actions,
        });
        Ok(())
    })?;
    let (window, pointer) = POINTER.get();
    let pointer = if window == source { pointer } else { 0 };
    let app = app.clone();
    let files: Vec<_> = uris
        .into_iter()
        .map(|uri| {
            let directory = file_path(&uri).is_some_and(|path| path.is_dir());
            (uri, directory)
        })
        .collect();
    // queue_after_input still runs inside XComponent's native mouse callback.
    // StartDrag can synchronously call that callback again; a separate TSFN task
    // is required to release both the native callback and Ability handler borrows.
    let dispatched = scheduler.dispatch(move || {
        // A surface might have disappeared while the action was queued.
        if !OUTGOING.with_borrow(|outgoing| outgoing.as_ref().is_some_and(|drag| drag.id == id)) {
            return;
        }
        let session = app
            .with_xcomponent_for(
                source.into_raw() as i64,
                openharmony_ability::NativeFileDrag::node_handle,
            )
            .map(|node| {
                openharmony_ability::NativeFileDrag::start_with_preview(
                    node,
                    pointer,
                    &files,
                    preview,
                    app.create_waker(),
                )
            });
        OUTGOING.with_borrow_mut(|outgoing| {
            if let Some(outgoing) = outgoing.as_mut().filter(|drag| drag.id == id) {
                match session {
                    Some(Ok(session)) => {
                        tracing::info!("OHOS native file drag started for {source:?}");
                        outgoing.session = Some(session);
                    }
                    error => {
                        if let Some(Err(error)) = error {
                            tracing::warn!("OHOS file drag: {error}");
                        }
                        outgoing.failed = true;
                    }
                }
            }
        });
        crate::frame::main_wake(&app).notify();
    });
    if dispatched.is_err() {
        cancel_outgoing(Some(source));
        return Err(TransferError::Failed);
    }
    Ok(id)
}
pub(crate) fn drain_outgoing() -> Option<(WindowId, WindowEvent)> {
    let outgoing = OUTGOING.with_borrow_mut(|outgoing| {
        if outgoing.as_ref().is_some_and(|drag| {
            drag.failed || drag.session.as_ref().is_some_and(|session| session.ended())
        }) {
            outgoing.take()
        } else {
            None
        }
    })?;
    let event = match outgoing
        .session
        .as_ref()
        .and_then(|session| session.result())
    {
        Some(openharmony_ability::NativeDragResult::Dropped(operation))
            if !outgoing.failed && outgoing.actions.contains(&winit_operation(operation)) =>
        {
            WindowEvent::OutgoingDragDropped {
                id: outgoing.id,
                action: Some(winit_operation(operation)),
            }
        }
        _ => WindowEvent::OutgoingDragCanceled { id: outgoing.id },
    };
    tracing::info!(
        "OHOS native file drag completed for {:?}: {event:?}",
        outgoing.source
    );
    Some((outgoing.source, event))
}
pub(crate) fn cancel_outgoing(window: Option<WindowId>) {
    let old = OUTGOING.with_borrow_mut(|outgoing| {
        if outgoing
            .as_ref()
            .is_some_and(|drag| window.is_none_or(|window| drag.source == window))
        {
            outgoing.take()
        } else {
            None
        }
    });
    drop(old);
}

fn winit_operation(operation: openharmony_ability::DragOperation) -> DndAction {
    match operation {
        openharmony_ability::DragOperation::Copy => DndAction::Copy,
        openharmony_ability::DragOperation::Move => DndAction::Move,
    }
}

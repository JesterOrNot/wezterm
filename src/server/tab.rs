use crate::config::{configuration, ConfigHandle};
use crate::mux::domain::DomainId;
use crate::mux::renderable::{Renderable, RenderableDimensions, StableCursorPosition};
use crate::mux::tab::{alloc_tab_id, Tab, TabId};
use crate::mux::Mux;
use crate::ratelim::RateLimiter;
use crate::server::client::Client;
use crate::server::codec::*;
use crate::server::domain::ClientInner;
use anyhow::anyhow;
use anyhow::bail;
use filedescriptor::Pipe;
use log::info;
use lru::LruCache;
use portable_pty::PtySize;
use promise::BrokenPromise;
use rangeset::*;
use std::cell::RefCell;
use std::cell::RefMut;
use std::collections::VecDeque;
use std::ops::Range;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use term::color::ColorPalette;
use term::{
    Clipboard, KeyCode, KeyModifiers, Line, MouseButton, MouseEvent, MouseEventKind,
    StableRowIndex, TerminalHost,
};
use termwiz::input::KeyEvent;
use url::Url;

struct MouseState {
    pending: AtomicBool,
    queue: VecDeque<MouseEvent>,
    client: Client,
    remote_tab_id: TabId,
}

impl MouseState {
    fn append(&mut self, event: MouseEvent) {
        if let Some(last) = self.queue.back_mut() {
            if last.modifiers == event.modifiers {
                if last.kind == MouseEventKind::Move
                    && event.kind == MouseEventKind::Move
                    && last.button == event.button
                {
                    // Collapse any interim moves and just buffer up
                    // the last of them
                    *last = event;
                    return;
                }

                // Similarly, for repeated wheel scrolls, add up the deltas
                // rather than swamping the queue
                match (&last.button, &event.button) {
                    (MouseButton::WheelUp(a), MouseButton::WheelUp(b)) => {
                        last.button = MouseButton::WheelUp(a + b);
                        return;
                    }
                    (MouseButton::WheelDown(a), MouseButton::WheelDown(b)) => {
                        last.button = MouseButton::WheelDown(a + b);
                        return;
                    }
                    _ => {}
                }
            }
        }
        self.queue.push_back(event);
        log::trace!("MouseEvent {}: queued", self.queue.len());
    }

    fn pop(&mut self) -> Option<MouseEvent> {
        if !self.pending.load(Ordering::SeqCst) {
            self.queue.pop_front()
        } else {
            None
        }
    }

    fn next(state: Rc<RefCell<Self>>) {
        let mut mouse = state.borrow_mut();
        if let Some(event) = mouse.pop() {
            let client = mouse.client.clone();

            let state = Rc::clone(&state);
            mouse.pending.store(true, Ordering::SeqCst);
            let remote_tab_id = mouse.remote_tab_id;

            promise::spawn::spawn(async move {
                client
                    .mouse_event(SendMouseEvent {
                        tab_id: remote_tab_id,
                        event,
                    })
                    .await
                    .ok();

                let mouse = state.borrow_mut();
                mouse.pending.store(false, Ordering::SeqCst);
                drop(mouse);

                Self::next(Rc::clone(&state));
                Ok::<(), anyhow::Error>(())
            });
        }
    }
}

pub struct ClientTab {
    client: Arc<ClientInner>,
    local_tab_id: TabId,
    remote_tab_id: TabId,
    renderable: RefCell<RenderableState>,
    writer: RefCell<TabWriter>,
    reader: Pipe,
    mouse: Rc<RefCell<MouseState>>,
    clipboard: RefCell<Option<Arc<dyn Clipboard>>>,
    mouse_grabbed: RefCell<bool>,
}

impl ClientTab {
    pub fn new(
        client: &Arc<ClientInner>,
        remote_tab_id: TabId,
        size: PtySize,
        title: &str,
    ) -> Self {
        let local_tab_id = alloc_tab_id();
        let writer = TabWriter {
            client: Arc::clone(client),
            remote_tab_id,
        };

        let mouse = Rc::new(RefCell::new(MouseState {
            remote_tab_id,
            client: client.client.clone(),
            pending: AtomicBool::new(false),
            queue: VecDeque::new(),
        }));

        let fetch_limiter =
            RateLimiter::new(|config| config.ratelimit_mux_line_prefetches_per_second);

        let render = RenderableState {
            inner: RefCell::new(RenderableInner {
                client: Arc::clone(client),
                remote_tab_id,
                local_tab_id,
                last_poll: Instant::now(),
                dead: false,
                poll_in_progress: AtomicBool::new(false),
                poll_interval: BASE_POLL_INTERVAL,
                cursor_position: StableCursorPosition::default(),
                dimensions: RenderableDimensions {
                    cols: size.cols as _,
                    viewport_rows: size.rows as _,
                    scrollback_rows: size.rows as _,
                    physical_top: 0,
                    scrollback_top: 0,
                },
                lines: LruCache::unbounded(),
                title: title.to_string(),
                working_dir: None,
                fetch_limiter,
            }),
        };

        let reader = Pipe::new().expect("Pipe::new failed");

        Self {
            client: Arc::clone(client),
            mouse,
            remote_tab_id,
            local_tab_id,
            renderable: RefCell::new(render),
            writer: RefCell::new(writer),
            reader,
            clipboard: RefCell::new(None),
            mouse_grabbed: RefCell::new(false),
        }
    }

    pub fn process_unilateral(&self, pdu: Pdu) -> anyhow::Result<()> {
        match pdu {
            Pdu::GetTabRenderChangesResponse(delta) => {
                *self.mouse_grabbed.borrow_mut() = delta.mouse_grabbed;
                self.renderable
                    .borrow()
                    .inner
                    .borrow_mut()
                    .apply_changes_to_surface(delta);
            }
            Pdu::SetClipboard(SetClipboard { clipboard, .. }) => {
                match self.clipboard.borrow().as_ref() {
                    Some(clip) => {
                        clip.set_contents(clipboard)?;
                    }
                    None => {
                        log::error!("ClientTab: Ignoring SetClipboard request {:?}", clipboard);
                    }
                }
            }
            _ => bail!("unhandled unilateral pdu: {:?}", pdu),
        };
        Ok(())
    }

    pub fn remote_tab_id(&self) -> TabId {
        self.remote_tab_id
    }
}

impl Tab for ClientTab {
    fn tab_id(&self) -> TabId {
        self.local_tab_id
    }
    fn renderer(&self) -> RefMut<dyn Renderable> {
        self.renderable.borrow_mut()
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.clipboard.borrow_mut().replace(Arc::clone(clipboard));
    }

    fn get_title(&self) -> String {
        let renderable = self.renderable.borrow();
        let inner = renderable.inner.borrow();
        inner.title.clone()
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        let client = Arc::clone(&self.client);
        let remote_tab_id = self.remote_tab_id;
        let data = text.to_owned();
        promise::spawn::spawn(async move {
            client
                .client
                .send_paste(SendPaste {
                    tab_id: remote_tab_id,
                    data,
                })
                .await
        });
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
        info!("made reader for ClientTab");
        Ok(Box::new(self.reader.read.try_clone()?))
    }

    fn writer(&self) -> RefMut<dyn std::io::Write> {
        self.writer.borrow_mut()
    }

    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        let render = self.renderable.borrow();
        let mut inner = render.inner.borrow_mut();

        let cols = size.cols as usize;
        let rows = size.rows as usize;

        if inner.dimensions.cols != cols || inner.dimensions.viewport_rows != rows {
            inner.dimensions.cols = cols;
            inner.dimensions.viewport_rows = rows;

            // Invalidate any cached rows on a resize
            inner.make_all_stale();

            let client = Arc::clone(&self.client);
            let remote_tab_id = self.remote_tab_id;
            promise::spawn::spawn(async move {
                client
                    .client
                    .resize(Resize {
                        tab_id: remote_tab_id,
                        size,
                    })
                    .await
            });
        }
        Ok(())
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let client = Arc::clone(&self.client);
        let remote_tab_id = self.remote_tab_id;
        promise::spawn::spawn(async move {
            client
                .client
                .key_down(SendKeyDown {
                    tab_id: remote_tab_id,
                    event: KeyEvent {
                        key,
                        modifiers: mods,
                    },
                })
                .await
        });
        Ok(())
    }

    fn mouse_event(&self, event: MouseEvent, _host: &mut dyn TerminalHost) -> anyhow::Result<()> {
        self.mouse.borrow_mut().append(event);
        MouseState::next(Rc::clone(&self.mouse));
        Ok(())
    }

    fn advance_bytes(&self, _buf: &[u8], _host: &mut dyn TerminalHost) {
        panic!("ClientTab::advance_bytes not impl");
    }

    fn is_dead(&self) -> bool {
        self.renderable.borrow().inner.borrow().dead
    }

    fn palette(&self) -> ColorPalette {
        let config = configuration();

        if let Some(scheme_name) = config.color_scheme.as_ref() {
            if let Some(palette) = config.color_schemes.get(scheme_name) {
                return palette.clone().into();
            }
        }

        config
            .colors
            .as_ref()
            .cloned()
            .map(Into::into)
            .unwrap_or_else(ColorPalette::default)
    }

    fn domain_id(&self) -> DomainId {
        self.client.local_domain_id
    }

    fn is_mouse_grabbed(&self) -> bool {
        *self.mouse_grabbed.borrow()
    }

    fn get_current_working_dir(&self) -> Option<Url> {
        self.renderable.borrow().inner.borrow().working_dir.clone()
    }
}

#[derive(Debug)]
enum LineEntry {
    // Up to date wrt. server and has been rendered at least once
    Line(Line),
    // Up to date wrt. server but needs to be rendered
    Dirty(Line),
    // Currently being downloaded from the server
    Fetching(Instant),
    // We have a version of the line locally and are treating it
    // as needing rendering because we are also in the process of
    // downloading a newer version from the server
    DirtyAndFetching(Line, Instant),
    // We have a local copy but it is stale and will need to be
    // fetched again
    Stale(Line),
}

impl LineEntry {
    fn kind(&self) -> (&'static str, Option<Instant>) {
        match self {
            Self::Line(_) => ("Line", None),
            Self::Dirty(_) => ("Dirty", None),
            Self::Fetching(since) => ("Fetching", Some(*since)),
            Self::DirtyAndFetching(_, since) => ("DirtyAndFetching", Some(*since)),
            Self::Stale(_) => ("Stale", None),
        }
    }
}

struct RenderableInner {
    client: Arc<ClientInner>,
    remote_tab_id: TabId,
    local_tab_id: TabId,
    last_poll: Instant,
    dead: bool,
    poll_in_progress: AtomicBool,
    poll_interval: Duration,

    cursor_position: StableCursorPosition,
    dimensions: RenderableDimensions,

    lines: LruCache<StableRowIndex, LineEntry>,
    title: String,
    working_dir: Option<Url>,

    fetch_limiter: RateLimiter,
}

struct RenderableState {
    inner: RefCell<RenderableInner>,
}

const MAX_POLL_INTERVAL: Duration = Duration::from_secs(30);
const BASE_POLL_INTERVAL: Duration = Duration::from_millis(20);

impl RenderableInner {
    fn apply_changes_to_surface(&mut self, delta: GetTabRenderChangesResponse) {
        self.poll_interval = BASE_POLL_INTERVAL;

        let mut dirty = RangeSet::new();
        for r in delta.dirty_lines {
            dirty.add_range(r.clone());
        }
        if delta.cursor_position != self.cursor_position {
            dirty.add(self.cursor_position.y);
            // But note that the server may have sent this in bonus_lines;
            // we'll address that below
            dirty.add(delta.cursor_position.y);
        }

        self.cursor_position = delta.cursor_position;
        self.dimensions = delta.dimensions;
        self.title = delta.title;
        self.working_dir = delta.working_dir.map(Into::into);

        let config = configuration();
        for (stable_row, line) in delta.bonus_lines.lines() {
            self.put_line(stable_row, line, &config, None);
            dirty.remove(stable_row);
        }

        if !dirty.is_empty() {
            Mux::get()
                .unwrap()
                .notify(crate::mux::MuxNotification::TabOutput(self.local_tab_id));
        }

        let now = Instant::now();
        let mut to_fetch = RangeSet::new();
        for r in dirty.iter() {
            for stable_row in r.clone() {
                // If a line is in the (probable) viewport region,
                // then we'll likely want to fetch it.
                // If it is outside that region, remove it from our cache
                // so that we'll fetch it on demand later.
                let fetchable = stable_row >= delta.dimensions.physical_top;
                let prior = self.lines.pop(&stable_row);
                let prior_kind = prior.as_ref().map(|e| e.kind());
                if !fetchable {
                    self.make_stale(stable_row);
                    continue;
                }
                to_fetch.add(stable_row);
                let entry = match prior {
                    Some(LineEntry::Fetching(_)) | None => LineEntry::Fetching(now),
                    Some(LineEntry::DirtyAndFetching(old, ..))
                    | Some(LineEntry::Stale(old))
                    | Some(LineEntry::Dirty(old))
                    | Some(LineEntry::Line(old)) => LineEntry::DirtyAndFetching(old, now),
                };
                log::trace!(
                    "row {} {:?} -> {:?} due to dirty and IN viewport",
                    stable_row,
                    prior_kind,
                    entry.kind()
                );
                self.lines.put(stable_row, entry);
            }
        }
        if !to_fetch.is_empty() {
            if self.fetch_limiter.non_blocking_admittance_check(1) {
                self.schedule_fetch_lines(to_fetch, now);
            } else {
                log::trace!("exceeded throttle, drop {:?}", to_fetch);
                for r in to_fetch.iter() {
                    for stable_row in r.clone() {
                        self.make_stale(stable_row);
                    }
                }
            }
        }
    }

    fn make_all_stale(&mut self) {
        let mut lines = LruCache::unbounded();
        while let Some((stable_row, entry)) = self.lines.pop_lru() {
            let entry = match entry {
                LineEntry::Dirty(old) | LineEntry::Stale(old) | LineEntry::Line(old) => {
                    LineEntry::Stale(old)
                }
                entry => entry,
            };
            lines.put(stable_row, entry);
        }
        self.lines = lines;
    }

    fn make_stale(&mut self, stable_row: StableRowIndex) {
        match self.lines.pop(&stable_row) {
            Some(LineEntry::Dirty(old))
            | Some(LineEntry::Stale(old))
            | Some(LineEntry::Line(old))
            | Some(LineEntry::DirtyAndFetching(old, _)) => {
                self.lines.put(stable_row, LineEntry::Stale(old));
            }
            Some(LineEntry::Fetching(_)) | None => {}
        }
    }

    fn put_line(
        &mut self,
        stable_row: StableRowIndex,
        mut line: Line,
        config: &ConfigHandle,
        fetch_start: Option<Instant>,
    ) {
        line.scan_and_create_hyperlinks(&config.hyperlink_rules);

        let entry = if let Some(fetch_start) = fetch_start {
            // If we're completing a fetch, only replace entries that were
            // set to fetching as part of our fetch.  If they are now longer
            // tagged that way, then someone came along after us and changed
            // the state, so we should leave it alone

            match self.lines.pop(&stable_row) {
                Some(LineEntry::DirtyAndFetching(_, then)) | Some(LineEntry::Fetching(then))
                    if fetch_start == then =>
                {
                    log::trace!("row {} fetch done -> Dirty", stable_row,);
                    LineEntry::Dirty(line)
                }
                Some(e) => {
                    // It changed since we started: leave it alone!
                    log::trace!(
                        "row {} {:?} changed since fetch started at {:?}, so leave it be",
                        stable_row,
                        e.kind(),
                        fetch_start
                    );
                    self.lines.put(stable_row, e);
                    return;
                }
                None => return,
            }
        } else {
            if let Some(LineEntry::Line(prior)) = self.lines.pop(&stable_row) {
                if prior == line {
                    LineEntry::Line(line)
                } else {
                    LineEntry::Dirty(line)
                }
            } else {
                LineEntry::Dirty(line)
            }
        };
        self.lines.put(stable_row, entry);
    }

    fn schedule_fetch_lines(&mut self, to_fetch: RangeSet<StableRowIndex>, now: Instant) {
        if to_fetch.is_empty() {
            return;
        }

        let local_tab_id = self.local_tab_id;
        log::trace!(
            "will fetch lines {:?} for remote tab id {} at {:?}",
            to_fetch,
            self.remote_tab_id,
            now,
        );

        let client = Arc::clone(&self.client);
        let remote_tab_id = self.remote_tab_id;

        promise::spawn::spawn(async move {
            let result = client
                .client
                .get_lines(GetLines {
                    tab_id: remote_tab_id,
                    lines: to_fetch.clone().into(),
                })
                .await;
            Self::apply_lines(local_tab_id, result, to_fetch, now)
        });
    }

    fn apply_lines(
        local_tab_id: TabId,
        result: anyhow::Result<GetLinesResponse>,
        to_fetch: RangeSet<StableRowIndex>,
        now: Instant,
    ) -> anyhow::Result<()> {
        let mux = Mux::get().unwrap();
        let tab = mux
            .get_tab(local_tab_id)
            .ok_or_else(|| anyhow!("no such tab {}", local_tab_id))?;
        if let Some(client_tab) = tab.downcast_ref::<ClientTab>() {
            let renderable = client_tab.renderable.borrow_mut();
            let mut inner = renderable.inner.borrow_mut();

            match result {
                Ok(result) => {
                    let config = configuration();
                    let lines = result.lines.lines();

                    log::trace!("fetch complete for {:?} at {:?}", to_fetch, now);
                    for (stable_row, line) in lines.into_iter() {
                        inner.put_line(stable_row, line, &config, Some(now));
                    }
                }
                Err(err) => {
                    log::error!("get_lines failed: {}", err);
                    for r in to_fetch.iter() {
                        for stable_row in r.clone() {
                            let entry = match inner.lines.pop(&stable_row) {
                                Some(LineEntry::Fetching(then)) if then == now => {
                                    // leave it popped
                                    continue;
                                }
                                Some(LineEntry::DirtyAndFetching(line, then)) if then == now => {
                                    // revert to just dirty
                                    LineEntry::Dirty(line)
                                }
                                Some(entry) => entry,
                                None => continue,
                            };
                            inner.lines.put(stable_row, entry);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn poll(&mut self) -> anyhow::Result<()> {
        if self.poll_in_progress.load(Ordering::SeqCst) {
            // We have a poll in progress
            return Ok(());
        }

        let interval = self.poll_interval;
        let interval = (interval + interval).min(MAX_POLL_INTERVAL);
        self.poll_interval = interval;

        let last = self.last_poll;
        if last.elapsed() < self.poll_interval {
            return Ok(());
        }

        self.last_poll = Instant::now();
        self.poll_in_progress.store(true, Ordering::SeqCst);
        let remote_tab_id = self.remote_tab_id;
        let local_tab_id = self.local_tab_id;
        let client = Arc::clone(&self.client);
        promise::spawn::spawn(async move {
            let alive = client
                .client
                .get_tab_render_changes(GetTabRenderChanges {
                    tab_id: remote_tab_id,
                })
                .await
                .is_ok();

            let mux = Mux::get().unwrap();
            let tab = mux
                .get_tab(local_tab_id)
                .ok_or_else(|| anyhow!("no such tab {}", local_tab_id))?;
            if let Some(client_tab) = tab.downcast_ref::<ClientTab>() {
                let renderable = client_tab.renderable.borrow_mut();
                let mut inner = renderable.inner.borrow_mut();

                inner.dead = !alive;
                inner.poll_in_progress.store(false, Ordering::SeqCst);
            }
            Ok::<(), anyhow::Error>(())
        });
        Ok(())
    }
}

impl Renderable for RenderableState {
    fn get_cursor_position(&self) -> StableCursorPosition {
        self.inner.borrow().cursor_position
    }

    fn get_lines(&mut self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let mut inner = self.inner.borrow_mut();
        let mut result = vec![];
        let mut to_fetch = RangeSet::new();
        let now = Instant::now();

        for idx in lines.clone() {
            let entry = match inner.lines.pop(&idx) {
                Some(LineEntry::Line(line)) => {
                    result.push(line.clone());
                    LineEntry::Line(line)
                }
                Some(LineEntry::Dirty(line)) => {
                    result.push(line.clone());
                    // Clear the dirty status as part of this retrieval
                    LineEntry::Line(line)
                }
                Some(LineEntry::DirtyAndFetching(line, then)) => {
                    result.push(line.clone());
                    LineEntry::DirtyAndFetching(line, then)
                }
                Some(LineEntry::Fetching(then)) => {
                    result.push(Line::with_width(inner.dimensions.cols));
                    LineEntry::Fetching(then)
                }
                Some(LineEntry::Stale(line)) => {
                    result.push(line.clone());
                    to_fetch.add(idx);
                    LineEntry::DirtyAndFetching(line, now)
                }
                None => {
                    result.push(Line::with_width(inner.dimensions.cols));
                    to_fetch.add(idx);
                    LineEntry::Fetching(now)
                }
            };
            inner.lines.put(idx, entry);
        }

        inner.schedule_fetch_lines(to_fetch, now);
        (lines.start, result)
    }

    fn get_dirty_lines(&self, lines: Range<StableRowIndex>) -> RangeSet<StableRowIndex> {
        let mut inner = self.inner.borrow_mut();
        if let Err(err) = inner.poll() {
            // We allow for BrokenPromise here for now; for a TLS backed
            // session it indicates that we'll retry.  For a local unix
            // domain session it is terminal... but we will detect that
            // terminal condition elsewhere
            if let Err(err) = err.downcast::<BrokenPromise>() {
                log::error!("remote tab poll failed: {}, marking as dead", err);
                inner.dead = true;
            }
        }

        let mut result = RangeSet::new();
        for r in lines {
            match inner.lines.get(&r) {
                None | Some(LineEntry::Dirty(_)) | Some(LineEntry::DirtyAndFetching(..)) => {
                    result.add(r);
                }
                _ => {}
            }
        }

        if !result.is_empty() {
            log::trace!("get_dirty_lines: {:?}", result);
        }

        result
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        self.inner.borrow().dimensions
    }
}

struct TabWriter {
    client: Arc<ClientInner>,
    remote_tab_id: TabId,
}

impl std::io::Write for TabWriter {
    fn write(&mut self, data: &[u8]) -> Result<usize, std::io::Error> {
        promise::spawn::block_on(self.client.client.write_to_tab(WriteToTab {
            tab_id: self.remote_tab_id,
            data: data.to_vec(),
        }))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{}", e)))?;
        Ok(data.len())
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
}

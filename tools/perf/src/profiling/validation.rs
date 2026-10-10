use crate::ToolResult;
use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs::File, io::BufReader, path::Path};

fn xml_input(path: &Path) -> ToolResult<impl std::io::BufRead> {
    let decoder = encoding_rs_io::DecodeReaderBytesBuilder::new()
        .utf8_passthru(true)
        .build(File::open(path)?);
    Ok(BufReader::new(decoder))
}

#[derive(Default)]
struct XmlDocument {
    depth: usize,
    started: bool,
}

impl XmlDocument {
    // quick-xml checks matching end names, but EOF does not imply root closure.
    fn observe(&mut self, event: &Event<'_>, root: &[u8]) -> ToolResult<()> {
        match event {
            Event::Start(element) | Event::Empty(element) => {
                if self.depth == 0 {
                    if self.started || element.local_name().as_ref() != root {
                        return Err(
                            "XML export must contain exactly one expected document root".into()
                        );
                    }
                    self.started = true;
                }
                if matches!(event, Event::Start(_)) {
                    self.depth = self
                        .depth
                        .checked_add(1)
                        .ok_or("XML element depth overflow")?;
                }
            }
            Event::End(_) => {
                self.depth = self
                    .depth
                    .checked_sub(1)
                    .ok_or("Unexpected XML closing element")?;
            }
            Event::Text(text) if self.depth == 0 && !text.decode()?.trim().is_empty() => {
                return Err("XML export contains text outside its root".into());
            }
            Event::CData(_) | Event::GeneralRef(_) if self.depth == 0 => {
                return Err("XML export contains data outside its root".into());
            }
            Event::Eof if !self.started || self.depth != 0 => {
                return Err("XML export is incomplete: missing root or unclosed elements".into());
            }
            _ => {}
        }
        Ok(())
    }
}

pub(super) struct Entity {
    pub(super) xpath: String,
    pub(super) label: String,
    detail_scope: Option<DetailScope>,
}

struct DetailScope {
    pid: u32,
    xpath: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TocElement {
    Root,
    Run,
    Info,
    Target,
    Tracks,
    Track,
    Details,
    Other,
}

fn attribute<R>(
    reader: &Reader<R>,
    element: &BytesStart<'_>,
    key: &[u8],
) -> ToolResult<Option<String>> {
    for item in element.attributes() {
        let item = item?;
        if item.key.as_ref() == key {
            return Ok(Some(
                item.decode_and_unescape_value(reader.decoder())?
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

fn quoted(value: &str) -> ToolResult<String> {
    if value.contains('\'') {
        return Err("Instrument entity name contains an unsupported XPath quote".into());
    }
    Ok(format!("'{value}'"))
}

fn toc_kind(name: &[u8]) -> TocElement {
    match name {
        b"trace-toc" => TocElement::Root,
        b"run" => TocElement::Run,
        b"info" => TocElement::Info,
        b"target" => TocElement::Target,
        b"tracks" => TocElement::Tracks,
        b"track" => TocElement::Track,
        b"details" => TocElement::Details,
        _ => TocElement::Other,
    }
}

fn select_instrument_table<R>(
    reader: &Reader<R>,
    element: &BytesStart<'_>,
    heap: bool,
    entities: &mut Vec<Entity>,
) -> ToolResult<()> {
    if let Some(schema) = attribute(reader, element, b"schema")? {
        let selected = if heap {
            schema.to_ascii_lowercase().contains("alloc")
        } else {
            matches!(
                schema.as_str(),
                "time-sample" | "time-profile" | "cpu-profile"
            )
        };
        if selected {
            entities.push(Entity {
                xpath: format!(
                    "/trace-toc/run[@number='1']/data/table[@schema={}]",
                    quoted(&schema)?
                ),
                label: schema,
                detail_scope: None,
            });
        }
    }
    Ok(())
}

fn select_allocation_detail<R>(
    reader: &Reader<R>,
    element: &BytesStart<'_>,
    track: Option<&str>,
    pid: u32,
    indexes: (usize, usize),
    entities: &mut Vec<Entity>,
) -> ToolResult<()> {
    if let (Some(track), Some(name)) = (track, attribute(reader, element, b"name")?)
        && track == "Allocations"
        && name == "Allocations List"
        && attribute(reader, element, b"kind")?.as_deref() == Some("table")
    {
        entities.push(Entity {
            xpath: format!(
                "/trace-toc/run[@number='1']/tracks/track[@name={}]/details/detail[@name={}]",
                quoted(track)?,
                quoted(&name)?
            ),
            label: name,
            detail_scope: Some(DetailScope {
                pid,
                xpath: format!(
                    "//trace-toc[1]/run[1]/tracks[1]/track[{}]/details[1]/detail[{}]",
                    indexes.0, indexes.1
                ),
            }),
        });
    }
    Ok(())
}

fn finish_instrument_entities(
    entities: &mut [Entity],
    target_found: bool,
    attached_target: (usize, usize, Option<u32>),
    pid: u32,
) -> ToolResult<()> {
    if !target_found {
        return Err("Instruments TOC does not identify the actual target PID".into());
    }
    if entities.is_empty() {
        return Err(
            "Instruments TOC exposes no CPU sample / allocation table for the selected template"
                .into(),
        );
    }
    for entity in entities.iter() {
        if entity.detail_scope.is_some() && attached_target != (1, 1, Some(pid)) {
            return Err(
                "Allocation detail requires exactly one attached target with the actual PID".into(),
            );
        }
    }
    entities.sort_by_key(|entity| match entity.label.as_str() {
        "Allocations List" | "time-sample" => 0,
        "time-profile" => 1,
        _ => 2,
    });
    Ok(())
}

#[derive(Default)]
struct TocTarget {
    found: bool,
    processes: usize,
    attached_pid: Option<u32>,
}

impl TocTarget {
    fn observe<R>(
        &mut self,
        reader: &Reader<R>,
        element: &BytesStart<'_>,
        pid_text: &str,
        selected_run: bool,
        parents: &[TocElement],
    ) -> ToolResult<()> {
        if selected_run
            && parents
                == [
                    TocElement::Root,
                    TocElement::Run,
                    TocElement::Info,
                    TocElement::Target,
                ]
        {
            self.processes += 1;
            if attribute(reader, element, b"type")?.as_deref() == Some("attached") {
                self.attached_pid =
                    attribute(reader, element, b"pid")?.and_then(|pid| pid.parse::<u32>().ok());
            }
        }
        self.found |= attribute(reader, element, b"pid")?.as_deref() == Some(pid_text);
        Ok(())
    }
}

pub(super) fn instrument_entities(toc: &str, pid: u32, heap: bool) -> ToolResult<Vec<Entity>> {
    let mut reader = Reader::from_str(toc);
    let mut entities = Vec::new();
    let mut target = TocTarget::default();
    let pid_text = pid.to_string();
    let mut track = None;
    let mut document = XmlDocument::default();
    let mut parents = Vec::new();
    let mut selected_run = false;
    let mut target_count = 0;
    let mut track_index = 0;
    let mut detail_index = 0;
    loop {
        let event = reader.read_event()?;
        document.observe(&event, b"trace-toc")?;
        match &event {
            Event::Start(element) | Event::Empty(element) => {
                let name = element.local_name();
                let kind = toc_kind(name.as_ref());
                if kind == TocElement::Run {
                    selected_run = attribute(&reader, element, b"number")?.as_deref() == Some("1");
                }
                if selected_run
                    && parents == [TocElement::Root, TocElement::Run, TocElement::Info]
                    && kind == TocElement::Target
                {
                    target_count += 1;
                }
                match name.as_ref() {
                    b"process" => {
                        target.observe(&reader, element, &pid_text, selected_run, &parents)?;
                    }
                    b"table" => {
                        select_instrument_table(&reader, element, heap, &mut entities)?;
                    }
                    b"track" => {
                        if selected_run
                            && parents == [TocElement::Root, TocElement::Run, TocElement::Tracks]
                        {
                            track_index += 1;
                            detail_index = 0;
                            track = attribute(&reader, element, b"name")?;
                        }
                    }
                    b"detail" if heap => {
                        let in_detail = selected_run
                            && parents
                                == [
                                    TocElement::Root,
                                    TocElement::Run,
                                    TocElement::Tracks,
                                    TocElement::Track,
                                    TocElement::Details,
                                ];
                        if in_detail {
                            detail_index += 1;
                            select_allocation_detail(
                                &reader,
                                element,
                                track.as_deref(),
                                pid,
                                (track_index, detail_index),
                                &mut entities,
                            )?;
                        }
                    }
                    _ => {}
                }
                if matches!(event, Event::Start(_)) {
                    parents.push(kind);
                } else if kind == TocElement::Track {
                    track = None;
                }
            }
            Event::End(_) => match parents.pop() {
                Some(TocElement::Track) => track = None,
                Some(TocElement::Run) => selected_run = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    finish_instrument_entities(
        &mut entities,
        target.found,
        (target_count, target.processes, target.attached_pid),
        pid,
    )?;
    Ok(entities)
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum InstrumentKind {
    Node,
    Row,
    Process,
    Thread,
    Pid,
    SampleTime,
    Weight,
    Size,
    Address,
    Backtrace,
    KperfBacktrace,
    Frame,
    TextAddresses,
    TextAddress,
    Column,
    ColumnName,
    #[default]
    Other,
}

fn instrument_kind(name: &[u8]) -> InstrumentKind {
    match name {
        b"node" => InstrumentKind::Node,
        b"row" => InstrumentKind::Row,
        b"process" => InstrumentKind::Process,
        b"thread" => InstrumentKind::Thread,
        b"pid" => InstrumentKind::Pid,
        b"sample-time" => InstrumentKind::SampleTime,
        b"weight" | b"cycle-weight" => InstrumentKind::Weight,
        b"backtrace" => InstrumentKind::Backtrace,
        b"kperf-bt" => InstrumentKind::KperfBacktrace,
        b"frame" => InstrumentKind::Frame,
        b"text-addresses" => InstrumentKind::TextAddresses,
        b"text-address" => InstrumentKind::TextAddress,
        b"col" => InstrumentKind::Column,
        b"name" => InstrumentKind::ColumnName,
        _ => InstrumentKind::Other,
    }
}

#[derive(Clone, Copy, Default)]
struct InstrumentValue {
    number: Option<u64>,
    pid: Option<u32>,
    payload: bool,
    conflict: bool,
}

#[derive(Default)]
struct CpuEvidence {
    sample_time: bool,
    weight: bool,
}

#[derive(Default)]
struct AllocationEvidence {
    size: bool,
    address: bool,
}

#[derive(Default)]
struct InstrumentElement {
    kind: InstrumentKind,
    id: Option<u64>,
    value: InstrumentValue,
    children: usize,
    cpu: CpuEvidence,
    allocation: AllocationEvidence,
    column: InstrumentKind,
}

impl InstrumentElement {
    fn attribute_pid(&mut self, pid: Option<u32>) {
        if let Some(pid) = pid {
            if self.value.pid.is_some_and(|existing| existing != pid) {
                self.value.conflict = true;
            }
            self.value.pid = Some(pid);
        }
    }
}

#[derive(Default)]
struct InstrumentRows {
    stack: Vec<InstrumentElement>,
    values: BTreeMap<(InstrumentKind, u64), InstrumentValue>,
    columns: Vec<InstrumentKind>,
    cpu_schema: bool,
    detail_pid: Option<u32>,
    rows: u64,
}

impl InstrumentRows {
    fn start_element<R>(
        &mut self,
        reader: &Reader<R>,
        element: &BytesStart<'_>,
        heap: bool,
        entity: Option<&Entity>,
    ) -> ToolResult<()> {
        if element.local_name().as_ref() == b"node" {
            self.values.clear();
            self.columns.clear();
            self.cpu_schema = false;
            // Attribute-only allocation detail rows inherit ownership solely
            // from the validated single-target TOC and this exact exported node.
            self.detail_pid = None;
            if heap
                && self.stack.len() == 1
                && let Some(scope) = entity.and_then(|entity| entity.detail_scope.as_ref())
                && attribute(reader, element, b"xpath")?.as_deref() == Some(scope.xpath.as_str())
            {
                self.detail_pid = Some(scope.pid);
            }
        }
        if element.local_name().as_ref() == b"schema" {
            self.cpu_schema = attribute(reader, element, b"name")?.is_some_and(|name| {
                matches!(
                    name.as_str(),
                    "time-sample" | "time-profile" | "cpu-profile"
                )
            });
        }
        let mut kind = instrument_kind(element.local_name().as_ref());
        if let Some(parent) = self.stack.last_mut()
            && parent.kind == InstrumentKind::Row
        {
            // Heap cell tags vary: bind Size/Address by the exported
            // schema's column order, not invented allocation tag names.
            if heap
                && let Some(column) = self.columns.get(parent.children)
                && matches!(column, InstrumentKind::Size | InstrumentKind::Address)
            {
                kind = *column;
            }
            parent.children += 1;
        }
        let mut parsed = InstrumentElement {
            kind,
            ..InstrumentElement::default()
        };
        let mut reference = None;
        let mut direct_pid = None;
        let mut address = None;
        for item in element.attributes() {
            let item = item?;
            if matches!(item.key.as_ref(), b"id" | b"ref" | b"pid" | b"addr") {
                let value = number(&item.decode_and_unescape_value(reader.decoder())?);
                match item.key.as_ref() {
                    b"id" => parsed.id = value,
                    b"ref" => reference = value,
                    b"pid" => direct_pid = value.and_then(|pid| u32::try_from(pid).ok()),
                    b"addr" => address = value,
                    _ => {}
                }
            }
        }
        if let Some(reference) = reference {
            parsed.value = self
                .values
                .get(&(kind, reference))
                .copied()
                .unwrap_or_default();
        }
        if kind == InstrumentKind::Process {
            parsed.attribute_pid(direct_pid);
        }
        if kind == InstrumentKind::Frame {
            parsed.value.payload |= address.is_some_and(|address| address > 0);
        }
        if heap
            && kind == InstrumentKind::Row
            && self.stack.len() == 2
            && self
                .stack
                .last()
                .is_some_and(|parent| parent.kind == InstrumentKind::Node)
            && let Some(pid) = self.detail_pid
        {
            parsed.attribute_pid(Some(pid));
            parsed.attribute_pid(direct_pid);
            let identifier = attribute(reader, element, b"identifier")?
                .and_then(|value| number(&value))
                .is_some_and(|value| value > 0);
            let timestamp =
                attribute(reader, element, b"timestamp")?.is_some_and(|value| !value.is_empty());
            if identifier && timestamp {
                parsed.allocation.address = attribute(reader, element, b"address")?
                    .and_then(|value| number(&value))
                    .is_some_and(|value| value > 0);
                parsed.allocation.size = attribute(reader, element, b"size")?
                    .and_then(|value| number(&value))
                    .is_some_and(|value| value > 0);
            }
        }
        self.stack.push(parsed);
        Ok(())
    }

    fn finish_element(&mut self, target: u32, heap: bool) -> ToolResult<()> {
        let element = self
            .stack
            .pop()
            .ok_or("Unexpected Instruments closing element")?;
        if let Some(id) = element.id {
            // Cache evidence-bearing types, not every symbol/binary in the trace.
            if !matches!(
                element.kind,
                InstrumentKind::Other
                    | InstrumentKind::Row
                    | InstrumentKind::Node
                    | InstrumentKind::Column
                    | InstrumentKind::ColumnName
            ) {
                self.values.insert((element.kind, id), element.value);
            }
        }
        if element.kind == InstrumentKind::Column {
            self.columns.push(element.column);
        }
        if element.kind == InstrumentKind::Row {
            let payload = if heap {
                element.allocation.size && element.allocation.address
            } else {
                self.cpu_schema
                    && element.cpu.sample_time
                    && (element.cpu.weight || element.value.payload)
            };
            if element.value.pid == Some(target) && !element.value.conflict && payload {
                self.rows = self
                    .rows
                    .checked_add(1)
                    .ok_or("Instrument row count overflow")?;
            }
        }
        if let Some(parent) = self.stack.last_mut() {
            match (parent.kind, element.kind) {
                (InstrumentKind::Process, InstrumentKind::Pid) => {
                    parent.attribute_pid(
                        element.value.number.and_then(|pid| u32::try_from(pid).ok()),
                    );
                }
                (
                    InstrumentKind::Thread | InstrumentKind::Row,
                    InstrumentKind::Process | InstrumentKind::Thread,
                ) => {
                    parent.attribute_pid(element.value.pid);
                    parent.value.conflict |= element.value.conflict;
                }
                (
                    InstrumentKind::Backtrace | InstrumentKind::KperfBacktrace,
                    InstrumentKind::Frame
                    | InstrumentKind::TextAddresses
                    | InstrumentKind::TextAddress,
                )
                | (
                    InstrumentKind::Row,
                    InstrumentKind::Backtrace | InstrumentKind::KperfBacktrace,
                ) => {
                    parent.value.payload |= element.value.payload;
                }
                (InstrumentKind::Row, InstrumentKind::SampleTime) => {
                    parent.cpu.sample_time |= element.value.number.is_some();
                }
                (InstrumentKind::Row, InstrumentKind::Weight) => {
                    parent.cpu.weight |= element.value.number.is_some_and(|value| value > 0);
                }
                (InstrumentKind::Row, InstrumentKind::Size) => {
                    parent.allocation.size |= element.value.number.is_some_and(|value| value > 0);
                }
                (InstrumentKind::Row, InstrumentKind::Address) => {
                    parent.allocation.address |=
                        element.value.number.is_some_and(|value| value > 0);
                }
                (InstrumentKind::Column, InstrumentKind::ColumnName) => {
                    parent.column = element.column;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

pub(super) fn instrument_rows(
    path: &Path,
    pid: u32,
    heap: bool,
    entity: Option<&Entity>,
) -> ToolResult<u64> {
    let mut reader = Reader::from_reader(xml_input(path)?);
    reader.config_mut().trim_text(true);
    let mut document = XmlDocument::default();
    let mut buffer = Vec::new();
    let mut evidence = InstrumentRows::default();
    loop {
        let event = reader.read_event_into(&mut buffer)?;
        document.observe(&event, b"trace-query-result")?;
        match &event {
            Event::Start(element) | Event::Empty(element) => {
                evidence.start_element(&reader, element, heap, entity)?;
                if matches!(event, Event::Empty(_)) {
                    evidence.finish_element(pid, heap)?;
                }
            }
            Event::Text(text) => {
                if let Some(element) = evidence.stack.last_mut() {
                    let text = text.decode()?;
                    match element.kind {
                        InstrumentKind::Pid
                        | InstrumentKind::SampleTime
                        | InstrumentKind::Weight
                        | InstrumentKind::Size
                        | InstrumentKind::Address
                        | InstrumentKind::TextAddress => {
                            element.value.number = number(&text);
                            if element.kind == InstrumentKind::TextAddress {
                                element.value.payload |=
                                    element.value.number.is_some_and(|value| value > 0);
                            }
                        }
                        InstrumentKind::TextAddresses => {
                            element.value.payload |= text
                                .split_whitespace()
                                .any(|address| number(address).is_some_and(|address| address > 0));
                        }
                        InstrumentKind::ColumnName => {
                            element.column = if text.eq_ignore_ascii_case("Size") {
                                InstrumentKind::Size
                            } else if text.eq_ignore_ascii_case("Address") {
                                InstrumentKind::Address
                            } else {
                                InstrumentKind::Other
                            };
                        }
                        _ => {}
                    }
                }
            }
            Event::End(_) => evidence.finish_element(pid, heap)?,
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    Ok(evidence.rows)
}

#[derive(Clone, Copy, Default)]
enum Provider {
    Cpu,
    Heap,
    Thread,
    #[default]
    Other,
}
#[derive(Clone, Copy, Default)]
enum Field {
    Opcode,
    Process,
    Thread,
    Size,
    Instruction,
    #[default]
    Other,
}
#[derive(Default)]
struct EtwEvent {
    provider: Provider,
    opcode: Option<u64>,
    pid: Option<u32>,
    tid: Option<u32>,
    payload_pid: Option<u32>,
    payload_tid: Option<u32>,
    size: u64,
    instruction: u64,
}

fn number(text: &str) -> Option<u64> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        text.parse().ok()
    }
}

fn provider(text: &str) -> Provider {
    let text = text.trim_matches(['{', '}']);
    if text.eq_ignore_ascii_case("ce1dbfb4-137e-4da6-87b0-3f59aa102cbc") {
        Provider::Cpu
    } else if text.eq_ignore_ascii_case("222962ab-6180-4b88-a825-346b75f2a24a") {
        Provider::Heap
    } else if text.eq_ignore_ascii_case("3d6fa8d1-fe05-11d0-9dda-00c04fd7ba7c") {
        Provider::Thread
    } else {
        Provider::Other
    }
}

fn field(name: &str) -> Field {
    if name.eq_ignore_ascii_case("ProcessId") {
        Field::Process
    } else if name.eq_ignore_ascii_case("ThreadId") || name.eq_ignore_ascii_case("TThreadId") {
        Field::Thread
    } else if name.eq_ignore_ascii_case("AllocSize")
        || name.eq_ignore_ascii_case("Allocation Size")
        || name.eq_ignore_ascii_case("Size")
    {
        Field::Size
    } else if name.eq_ignore_ascii_case("InstructionPointer") {
        Field::Instruction
    } else {
        Field::Other
    }
}

fn event_element(element: &BytesStart<'_>, event: &mut EtwEvent) -> ToolResult<Field> {
    let local = element.local_name();
    if local.as_ref() == b"Opcode" {
        return Ok(Field::Opcode);
    }
    let mut active = Field::Other;
    for item in element.attributes() {
        let item = item?;
        let value = std::str::from_utf8(item.value.as_ref())?;
        match (local.as_ref(), item.key.as_ref()) {
            (b"Provider", b"Guid") => event.provider = provider(value),
            (b"Execution", b"ProcessID") => {
                event.pid = number(value).and_then(|value| u32::try_from(value).ok());
            }
            (b"Execution", b"ThreadID") => {
                event.tid = number(value).and_then(|value| u32::try_from(value).ok());
            }
            (b"Data", b"Name") => active = field(value),
            _ => {}
        }
    }
    Ok(active)
}

pub(super) fn etl_summary(path: &Path, target: u32, heap: bool) -> ToolResult<Value> {
    let mut reader = Reader::from_reader(xml_input(path)?);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut event = EtwEvent::default();
    let mut active = Field::Other;
    let mut threads = BTreeMap::new();
    let mut samples = 0_u64;
    let mut allocations = 0_u64;
    let mut bytes = 0_u64;
    let mut document = XmlDocument::default();
    loop {
        let xml_event = reader.read_event_into(&mut buffer)?;
        document.observe(&xml_event, b"Events")?;
        match xml_event {
            Event::Start(element) | Event::Empty(element) => {
                active = event_element(&element, &mut event)?;
            }
            Event::Text(text) => {
                if let Some(value) = number(&text.decode()?) {
                    match active {
                        Field::Opcode => event.opcode = Some(value),
                        Field::Process => event.payload_pid = u32::try_from(value).ok(),
                        Field::Thread => event.payload_tid = u32::try_from(value).ok(),
                        Field::Size => event.size = value,
                        Field::Instruction => event.instruction = value,
                        Field::Other => {}
                    }
                }
            }
            Event::End(element) => {
                active = Field::Other;
                if element.local_name().as_ref() == b"Event" {
                    match event.provider {
                        Provider::Thread => {
                            if let (Some(tid), Some(pid)) = (event.payload_tid, event.payload_pid) {
                                if matches!(event.opcode, Some(1 | 3)) {
                                    threads.insert(tid, pid);
                                } else if matches!(event.opcode, Some(2 | 4)) {
                                    threads.remove(&tid);
                                }
                            }
                        }
                        Provider::Cpu if event.opcode == Some(46) && event.instruction != 0 => {
                            let attributed = event.pid == Some(target)
                                || event
                                    .payload_tid
                                    .or(event.tid)
                                    .is_some_and(|tid| threads.get(&tid) == Some(&target));
                            if attributed {
                                samples =
                                    samples.checked_add(1).ok_or("CPU sample count overflow")?;
                            }
                        }
                        Provider::Heap
                            if event.opcode == Some(33)
                                && event.pid == Some(target)
                                && event.size > 0 =>
                        {
                            allocations = allocations
                                .checked_add(1)
                                .ok_or("Allocation event count overflow")?;
                            bytes = bytes
                                .checked_add(event.size)
                                .ok_or("Native allocation byte total overflow")?;
                        }
                        _ => {}
                    }
                    event = EtwEvent::default();
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if (heap && allocations == 0) || (!heap && samples == 0) {
        return Err("ETL contains no decoded target-PID sampled CPU / heap allocation events; session metadata is not a successful profile".into());
    }
    Ok(
        json!({"target_pid": target, "cpu_sample_records": samples, "allocation_records": allocations, "total_allocated_bytes": if heap {Some(bytes)} else {None}, "measurement": "Decoded ETW provider/opcode records, not RSS or clean latency", "decoder": "Windows tracerpt XML; Windows SDK wmicore.mof provider/opcode identities"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CPU: &str = "ce1dbfb4-137e-4da6-87b0-3f59aa102cbc";
    const HEAP: &str = "222962ab-6180-4b88-a825-346b75f2a24a";
    const THREAD: &str = "3d6fa8d1-fe05-11d0-9dda-00c04fd7ba7c";

    fn event(provider: &str, opcode: u32, pid: u32, data: &str) -> String {
        format!(
            "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Guid='{{{provider}}}'/><Opcode>{opcode}</Opcode><Execution ProcessID='{pid}' ThreadID='17'/></System><EventData>{data}</EventData></Event>"
        )
    }

    fn trace(events: &str) -> ToolResult<tempfile::NamedTempFile> {
        let mut file = tempfile::NamedTempFile::new()?;
        write!(file, "<Events>{events}</Events>")?;
        Ok(file)
    }

    #[test]
    fn cpu_records_resolve_kernel_thread_attribution() -> ToolResult<()> {
        let thread = event(
            THREAD,
            3,
            0,
            "<Data Name='ProcessId'>42</Data><Data Name='TThreadId'>17</Data>",
        );
        let sample = event(
            CPU,
            46,
            0,
            "<Data Name='InstructionPointer'>0x1000</Data><Data Name='ThreadId'>17</Data>",
        );
        let unrelated = event(
            CPU,
            46,
            99,
            "<Data Name='InstructionPointer'>0x2000</Data><Data Name='ThreadId'>19</Data>",
        );
        let file = trace(&(thread + &sample + &unrelated))?;
        assert_eq!(
            etl_summary(file.path(), 42, false)?["cpu_sample_records"],
            1
        );
        assert!(etl_summary(file.path(), 88, false).is_err());
        Ok(())
    }

    #[test]
    fn heap_records_require_allocator_provider_pid_and_size() -> ToolResult<()> {
        let valid = event(HEAP, 33, 42, "<Data Name='AllocSize'>128</Data>");
        let unrelated = event(HEAP, 33, 99, "<Data Name='AllocSize'>256</Data>");
        let file = trace(&(valid + &unrelated))?;
        let summary = etl_summary(file.path(), 42, true)?;
        assert_eq!(summary["allocation_records"], 1);
        assert_eq!(summary["total_allocated_bytes"], 128);
        assert!(etl_summary(file.path(), 42, false).is_err());
        let metadata = trace(&event(
            HEAP,
            32,
            42,
            "<Data Name='HeapHandle'>0x1000</Data>",
        ))?;
        assert!(etl_summary(metadata.path(), 42, true).is_err());
        Ok(())
    }

    #[test]
    fn windows_utf16_exports_decode_without_losing_events() -> ToolResult<()> {
        let xml = format!(
            "<Events>{}</Events>",
            event(CPU, 46, 42, "<Data Name='InstructionPointer'>0x1000</Data>")
        );
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(&[0xff, 0xfe])?;
        for unit in xml.encode_utf16() {
            file.write_all(&unit.to_le_bytes())?;
        }
        assert_eq!(
            etl_summary(file.path(), 42, false)?["cpu_sample_records"],
            1
        );
        Ok(())
    }

    #[test]
    fn instruments_discovers_real_allocation_detail_and_checks_pid() -> ToolResult<()> {
        let toc = "<trace-toc><run number='1'><info><target><process type='attached' name='bend2-lsp' pid='42'/></target></info><tracks><track name='Allocations'><details><detail name='Allocations List' kind='table'/></details></track></tracks></run></trace-toc>";
        let entities = instrument_entities(toc, 42, true)?;
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].label, "Allocations List");
        assert!(instrument_entities(toc, 43, true).is_err());
        assert!(instrument_entities(toc, 42, false).is_err());
        Ok(())
    }

    fn xml_file(xml: &str) -> ToolResult<tempfile::NamedTempFile> {
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(xml.as_bytes())?;
        Ok(file)
    }

    fn instruments_export(rows: &str) -> String {
        format!(
            "<trace-query-result><node><schema name='time-profile'/>{rows}</node></trace-query-result>"
        )
    }

    // Reduced from Instruments 16.0 exports in hosted run 38071554112,
    // artifact 11677094803: profiles/discovery-10000/{native,native-heap}/native-table.xml.
    // Keep the actual CPU value types/references and attribute-only heap rows.
    const HOSTED_CPU: &str = r#"<trace-query-result>
<node xpath='//trace-toc[1]/run[1]/data[1]/table[39]'><schema name="time-sample"><col><mnemonic>time</mnemonic><name>Timestamp</name><engineering-type>sample-time</engineering-type></col><col><mnemonic>thread</mnemonic><name>Thread</name><engineering-type>thread</engineering-type></col><col><mnemonic>cp-user-callstack</mnemonic><name>User Callstack ID</name><engineering-type>kperf-bt</engineering-type></col></schema>
<row><sample-time id="1">1066524416</sample-time><thread id="2"><tid id="3">55192</tid><process id="4"><pid id="5">18854</pid></process></thread><sentinel/><thread-state id="7">Blocked</thread-state><sentinel/><kperf-bt id="8"><text-addresses id="9">4377765984 4375091312 4374503056 4375462048 6600682392</text-addresses><text-address id="10">6604231628</text-address><process ref="4"/><register-content id="11">0</register-content></kperf-bt><time-sample-kind id="12">3</time-sample-kind></row>
<row><sample-time ref="1"/><thread id="13"><tid id="14">55213</tid><process ref="4"/></thread><sentinel/><thread-state ref="7"/><sentinel/><kperf-bt id="15"><text-addresses id="16">4377794464 4377784648 4377778584 4377724608 4377773472 4377674532 6604487624</text-addresses><text-address id="17">6604242180</text-address><process ref="4"/><register-content id="18">6604467072</register-content></kperf-bt><time-sample-kind ref="12"/></row>
<row><sample-time ref="1"/><thread id="19"><tid id="20">55214</tid><process ref="4"/></thread><sentinel/><thread-state ref="7"/><sentinel/><kperf-bt id="21"><text-addresses id="22">4377784816 4377778584 4377724608 4377773472 4377674532 6604487624</text-addresses><text-address ref="10"/><process ref="4"/><register-content ref="18"/></kperf-bt><time-sample-kind ref="12"/></row>
<row><sample-time ref="1"/><thread id="23"><tid id="24">55215</tid><process ref="4"/></thread><sentinel/><thread-state ref="7"/><sentinel/><kperf-bt ref="21"/><time-sample-kind ref="12"/></row>
</node></trace-query-result>"#;

    const HOSTED_HEAP_TOC: &str = r#"<trace-toc><run number="1"><info><target><process type="attached" name="bend2-lsp" pid="22113"/></target></info><processes><process name="kernel.release.vmapple" pid="0"/><process name="bend2-lsp" pid="22113"/></processes><tracks><track name="Allocations"><details><detail name="Statistics" kind="table"/><detail name="Allocations List" kind="table"/></details></track><track name="VM Tracker"><details><detail name="Regions Map" kind="table"/></details></track></tracks></run></trace-toc>"#;

    const HOSTED_HEAP: &str = r#"<trace-query-result>
<node xpath='//trace-toc[1]/run[1]/tracks[1]/track[1]/details[1]/detail[2]'><row address="0x600002b00ff0" category="Malloc 80 Bytes" live="true" responsible-caller="&lt;Call stack limit reached&gt;" size="80" identifier="2164" responsible-library="" timestamp="00:00.000.000" index="0"/>
<row address="0x600002b01090" category="Malloc 80 Bytes" live="true" responsible-caller="&lt;Call stack limit reached&gt;" size="80" identifier="2166" responsible-library="" timestamp="00:00.000.000" index="1"/>
</node></trace-query-result>"#;

    #[test]
    fn instruments_hosted_time_samples_resolve_kperf_payloads() -> ToolResult<()> {
        let file = xml_file(HOSTED_CPU)?;
        assert_eq!(instrument_rows(file.path(), 18854, false, None)?, 4);
        assert_eq!(instrument_rows(file.path(), 99, false, None)?, 0);
        assert_eq!(instrument_rows(file.path(), 18854, true, None)?, 0);
        for suffix in ["</trace-query-result>", "</node></trace-query-result>"] {
            let file = xml_file(
                HOSTED_CPU
                    .strip_suffix(suffix)
                    .ok_or("Missing fixture suffix")?,
            )?;
            assert!(instrument_rows(file.path(), 18854, false, None).is_err());
        }
        Ok(())
    }

    #[test]
    fn instruments_kperf_process_cannot_supply_row_attribution() -> ToolResult<()> {
        let rows = [
            "<row><sample-time>100</sample-time><kperf-bt id='8'><text-address id='10'>6604231628</text-address><process id='4'><pid>18854</pid></process></kperf-bt></row>",
            "<row><sample-time>101</sample-time><thread><process><pid>99</pid></process></thread><kperf-bt ref='8'/></row>",
            "<row><sample-time>102</sample-time><thread><process ref='4'/></thread><kperf-bt><text-address ref='10'/></kperf-bt></row>",
            "<row><sample-time>103</sample-time><thread><process ref='4'/></thread><backtrace ref='8'/></row>",
            "<row><sample-time>104</sample-time><thread><process ref='4'/></thread><kperf-bt ref='999'/></row>",
            "<row><sample-time>105</sample-time><thread><process ref='4'/></thread><kperf-bt><process ref='4'/><register-content>6604231628</register-content></kperf-bt></row>",
        ];
        let file = xml_file(&instruments_export(&rows.concat()))?;
        // Only the third row has both an attributed thread and an actual PC.
        assert_eq!(instrument_rows(file.path(), 18854, false, None)?, 1);
        Ok(())
    }

    #[test]
    fn instruments_hosted_allocation_attributes_require_exact_target_scope() -> ToolResult<()> {
        let entities = instrument_entities(HOSTED_HEAP_TOC, 22113, true)?;
        let entity = &entities[0];
        let file = xml_file(HOSTED_HEAP)?;
        assert_eq!(instrument_rows(file.path(), 22113, true, Some(entity))?, 2);
        assert_eq!(instrument_rows(file.path(), 99, true, Some(entity))?, 0);
        assert_eq!(instrument_rows(file.path(), 22113, true, None)?, 0);
        assert_eq!(instrument_rows(file.path(), 22113, false, Some(entity))?, 0);
        for (from, to) in [
            ("detail[2]", "detail[1]"),
            ("track[1]", "track[2]"),
            ("run[1]", "run[2]"),
        ] {
            let file = xml_file(&HOSTED_HEAP.replace(from, to))?;
            assert_eq!(instrument_rows(file.path(), 22113, true, Some(entity))?, 0);
        }
        for suffix in ["</trace-query-result>", "</node></trace-query-result>"] {
            let file = xml_file(
                HOSTED_HEAP
                    .strip_suffix(suffix)
                    .ok_or("Missing fixture suffix")?,
            )?;
            assert!(instrument_rows(file.path(), 22113, true, Some(entity)).is_err());
        }
        Ok(())
    }

    #[test]
    fn instruments_allocation_detail_rejects_ambiguous_or_metadata_targets() {
        for toc in [
            HOSTED_HEAP_TOC.replace("type=\"attached\"", "type=\"launched\""),
            HOSTED_HEAP_TOC.replace("type=\"attached\"", ""),
            HOSTED_HEAP_TOC.replace(
                "type=\"attached\" name=\"bend2-lsp\" pid=\"22113\"",
                "type=\"attached\" name=\"other\" pid=\"99\"",
            ),
            HOSTED_HEAP_TOC.replace(
                "</target>",
                "<process type=\"attached\" pid=\"99\"/></target>",
            ),
            HOSTED_HEAP_TOC.replace(
                "</target>",
                "</target><target><process type=\"attached\" pid=\"99\"/></target>",
            ),
            HOSTED_HEAP_TOC
                .replace("<target>", "<metadata>")
                .replace("</target>", "</metadata>"),
        ] {
            assert!(instrument_entities(&toc, 22113, true).is_err());
        }
    }

    #[test]
    fn instruments_allocation_detail_attributes_are_not_metadata_evidence() -> ToolResult<()> {
        let entities = instrument_entities(HOSTED_HEAP_TOC, 22113, true)?;
        let entity = &entities[0];
        for xml in [
            HOSTED_HEAP.replace("size=\"80\"", "size=\"0\""),
            HOSTED_HEAP.replace("size=\"80\"", "size=\"invalid\""),
            HOSTED_HEAP
                .replace("address=\"0x600002b00ff0\"", "address=\"0\"")
                .replace("address=\"0x600002b01090\"", "address=\"0\""),
            HOSTED_HEAP
                .replace("identifier=\"2164\"", "")
                .replace("identifier=\"2166\"", ""),
            HOSTED_HEAP.replace("timestamp=\"00:00.000.000\"", ""),
            HOSTED_HEAP.replace("<row ", "<row pid=\"99\" "),
            HOSTED_HEAP
                .replace("<row ", "<metadata><row ")
                .replace("/>\n", "/></metadata>\n"),
        ] {
            let file = xml_file(&xml)?;
            assert_eq!(instrument_rows(file.path(), 22113, true, Some(entity))?, 0);
        }
        let xml = HOSTED_HEAP.replace("</node>", "</node><node xpath='metadata'><row address='0x1000' size='80' identifier='1' timestamp='0'/></node>");
        let file = xml_file(&xml)?;
        assert_eq!(instrument_rows(file.path(), 22113, true, Some(entity))?, 2);
        Ok(())
    }

    // Reduced from Microsoft's InstrumentsProcessorTests/TestData/system_trace.xml:
    // https://github.com/microsoft/Microsoft-Performance-Tools-Apple/blob/312a5106069bc5a49a60b3a35f5bd32e66268a61/InstrumentsProcessorTests/TestData/system_trace.xml
    fn instrument_sample(pid: u32) -> String {
        format!(
            "<row><sample-time id='1'>9714708</sample-time><thread id='2'><tid id='3'>568</tid><process id='4'><pid id='5'>{pid}</pid></process></thread><process ref='4'/><weight id='9'>1000000</weight><sentinel/></row>"
        )
    }

    #[test]
    fn instruments_target_in_toc_cannot_validate_other_process_samples() -> ToolResult<()> {
        let toc = "<trace-toc><run number='1'><info><target><process pid='42'/></target></info><data><table schema='time-profile'/></data></run></trace-toc>";
        assert_eq!(instrument_entities(toc, 42, false)?.len(), 1);
        let file = xml_file(&instruments_export(&instrument_sample(99)))?;
        assert_eq!(instrument_rows(file.path(), 42, false, None)?, 0);
        Ok(())
    }

    #[test]
    fn instruments_metadata_rows_are_not_cpu_or_allocation_evidence() -> ToolResult<()> {
        let file = xml_file(&instruments_export(
            "<row><process id='4'><pid>42</pid></process></row><row/>",
        ))?;
        assert_eq!(instrument_rows(file.path(), 42, false, None)?, 0);
        assert_eq!(instrument_rows(file.path(), 42, true, None)?, 0);
        Ok(())
    }

    #[test]
    fn instruments_rejects_truncated_export_after_complete_sample() -> ToolResult<()> {
        let xml = instruments_export(&instrument_sample(42));
        for suffix in [
            "</trace-query-result>",
            "</node></trace-query-result>",
            "</row></node></trace-query-result>",
        ] {
            let file = xml_file(xml.strip_suffix(suffix).ok_or("Missing fixture suffix")?)?;
            assert!(
                instrument_rows(file.path(), 42, false, None).is_err(),
                "accepted export missing {suffix}"
            );
        }
        Ok(())
    }

    #[test]
    fn instruments_toc_requires_complete_root_and_elements() {
        let valid = "<trace-toc><run number='1'><process pid='42'/><data><table schema='time-profile'/></data></run></trace-toc>";
        assert!(instrument_entities(valid, 42, false).is_ok());
        for suffix in [
            "</trace-toc>",
            "</run></trace-toc>",
            "</data></run></trace-toc>",
        ] {
            assert!(
                instrument_entities(
                    valid.strip_suffix(suffix).expect("fixture suffix"),
                    42,
                    false
                )
                .is_err()
            );
        }
    }

    #[test]
    fn etl_rejects_truncated_document_after_complete_target_event() -> ToolResult<()> {
        let sample = event(CPU, 46, 42, "<Data Name='InstructionPointer'>0x1000</Data>");
        let file = xml_file(&format!("<Events>{sample}"))?;
        assert!(etl_summary(file.path(), 42, false).is_err());
        let file = xml_file(&format!("<Events>{sample}<Event><System>"))?;
        assert!(etl_summary(file.path(), 42, false).is_err());
        let allocation = event(HEAP, 33, 42, "<Data Name='AllocSize'>128</Data>");
        let file = xml_file(&format!("<Events>{allocation}"))?;
        assert!(etl_summary(file.path(), 42, true).is_err());
        Ok(())
    }

    #[test]
    fn instruments_resolves_process_thread_pid_and_payload_references() -> ToolResult<()> {
        let definition = instrument_sample(42);
        let references = "<row><sample-time ref='1'/><thread ref='2'/><process ref='4'/><weight ref='9'/><sentinel/></row>";
        let thread_only = "<row><sample-time>20000000</sample-time><thread ref='2'/><weight ref='9'/><sentinel/></row>";
        let pid_reference = "<row><sample-time>30000000</sample-time><process id='10'><pid ref='5'/></process><weight ref='9'/><sentinel/></row>";
        let unrelated = "<row><sample-time>40000000</sample-time><process><pid>99</pid></process><weight ref='9'/></row>";
        let file = xml_file(&instruments_export(
            &(definition + references + thread_only + pid_reference + unrelated),
        ))?;
        assert_eq!(instrument_rows(file.path(), 42, false, None)?, 4);
        Ok(())
    }

    #[test]
    fn instruments_does_not_attribute_row_from_backtrace_process() -> ToolResult<()> {
        let row = "<row><sample-time>100</sample-time><process><pid>99</pid></process><weight>1000000</weight><backtrace><process><pid>42</pid></process><text-addresses>4331406713</text-addresses></backtrace></row>";
        let file = xml_file(&instruments_export(row))?;
        assert_eq!(instrument_rows(file.path(), 42, false, None)?, 0);
        Ok(())
    }

    #[test]
    fn instruments_rejects_dangling_and_wrong_type_attribution_references() -> ToolResult<()> {
        let row = "<row><sample-time id='4'>42</sample-time><process ref='4'/><thread ref='17'/><weight>1000000</weight></row>";
        let file = xml_file(&instruments_export(row))?;
        assert_eq!(instrument_rows(file.path(), 42, false, None)?, 0);
        Ok(())
    }

    #[test]
    fn instruments_allocation_columns_require_target_address_and_positive_size() -> ToolResult<()> {
        // Exercise schema-driven cells without claiming an undocumented heap tag
        // spelling. The exported schema declares the meaning and order of cells.
        // Size/Address are native allocation-list columns (Apple MemoryPlugin).
        let schema = "<schema><col><name>Process</name></col><col><name>Address</name></col><col><name>Size</name></col></schema>";
        let definition = "<row><process id='1'><pid id='2'>42</pid></process><value id='3'>0x1000</value><value id='4'>128</value></row>";
        let reference = "<row><process ref='1'/><value ref='3'/><value ref='4'/></row>";
        let unrelated =
            "<row><process><pid>99</pid></process><value>0x2000</value><value>256</value></row>";
        let no_size = "<row><process ref='1'/><value ref='3'/><sentinel/></row>";
        let zero_size = "<row><process ref='1'/><value ref='3'/><value>0</value></row>";
        let no_address = "<row><process ref='1'/><sentinel/><value ref='4'/></row>";
        let xml = format!(
            "<trace-query-result><node>{schema}{definition}{reference}{unrelated}{no_size}{zero_size}{no_address}</node></trace-query-result>"
        );
        let file = xml_file(&xml)?;
        assert_eq!(instrument_rows(file.path(), 42, true, None)?, 2);
        assert_eq!(instrument_rows(file.path(), 99, true, None)?, 1);
        assert_eq!(instrument_rows(file.path(), 43, true, None)?, 0);
        assert_eq!(instrument_rows(file.path(), 42, false, None)?, 0);
        let file = xml_file(
            xml.strip_suffix("</trace-query-result>")
                .ok_or("Missing fixture suffix")?,
        )?;
        assert!(instrument_rows(file.path(), 42, true, None).is_err());
        Ok(())
    }

    #[test]
    fn instruments_cpu_requires_sample_payload_and_consistent_attribution() -> ToolResult<()> {
        let timestamp_only =
            "<row><sample-time>0</sample-time><process><pid>42</pid></process></row>";
        let empty_payload = "<row><sample-time>100</sample-time><process><pid>42</pid></process><weight>0</weight><backtrace/></row>";
        let conflicting = "<row><sample-time>200</sample-time><thread><process><pid>99</pid></process></thread><process><pid>42</pid></process><weight>1000000</weight></row>";
        let file = xml_file(&instruments_export(
            &(timestamp_only.to_owned() + empty_payload + conflicting),
        ))?;
        assert_eq!(instrument_rows(file.path(), 42, false, None)?, 0);
        let frame = "<row><sample-time>0</sample-time><process id='1'><pid>42</pid></process><backtrace id='2'><frame id='3' addr='0x1000'/></backtrace></row>";
        let referenced_frame = "<row><sample-time>1</sample-time><process ref='1'/><backtrace><frame ref='3'/></backtrace></row>";
        let referenced_stack =
            "<row><sample-time>2</sample-time><process ref='1'/><backtrace ref='2'/></row>";
        let addresses = "<row><sample-time>3</sample-time><process ref='1'/><backtrace><process ref='1'/><text-addresses id='4'>4331406713 4331406716</text-addresses></backtrace></row>";
        let referenced_addresses = "<row><sample-time>4</sample-time><process ref='1'/><backtrace><text-addresses ref='4'/></backtrace></row>";
        let file = xml_file(&instruments_export(
            &(frame.to_owned()
                + referenced_frame
                + referenced_stack
                + addresses
                + referenced_addresses),
        ))?;
        assert_eq!(instrument_rows(file.path(), 42, false, None)?, 5);
        Ok(())
    }

    #[test]
    fn native_xml_rejects_absent_or_multiple_document_roots() -> ToolResult<()> {
        assert!(instrument_entities("", 42, false).is_err());
        let sample = instruments_export(&instrument_sample(42));
        let file = xml_file(&(sample.clone() + &sample))?;
        assert!(instrument_rows(file.path(), 42, false, None).is_err());
        let file = xml_file(&(sample + "trailing text"))?;
        assert!(instrument_rows(file.path(), 42, false, None).is_err());
        let event = event(CPU, 46, 42, "<Data Name='InstructionPointer'>0x1000</Data>");
        let file = xml_file(&format!("<Events>{event}</Events><Events/>"))?;
        assert!(etl_summary(file.path(), 42, false).is_err());
        Ok(())
    }
}

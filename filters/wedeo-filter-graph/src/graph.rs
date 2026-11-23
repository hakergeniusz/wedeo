//! Functional filter graph.
//!
//! Port of the graph-level logic in FFmpeg's `libavfilter/avfilter.c`,
//! `libavfilter/avfiltergraph.c`, `libavfilter/buffersrc.c` and
//! `libavfilter/buffersink.c`: nodes are created from the filter registry,
//! pads are validated and linked, frames travel through per-link queues and
//! are transformed by each node's `filter_frame`.
//!
//! The frozen `wedeo_filter::graph::FilterGraph` stub in the framework crate
//! is intentionally left untouched; this module owns a fully functional
//! `Graph` type instead.

use std::collections::{HashSet, VecDeque};

use wedeo_core::error::{Error, Result};
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_filter::Filter;
use wedeo_filter::graph::FilterLinkConfig;
use wedeo_filter::registry::{filters, find_filter_by_name};

/// Identifier of a node within a [`Graph`].
pub type NodeId = usize;
/// Identifier of a link within a [`Graph`].
pub type LinkId = usize;

/// Upper bound on `filter_frame` calls in one [`Graph::run`] invocation.
///
/// FFmpeg relies on `ff_filter_graph_run_once` repeatedly making progress;
/// a wedeo graph has no scheduler, so a hard budget keeps a misbehaving
/// filter from hanging the caller forever.
const MAX_RUN_STEPS: usize = 1_000_000;

/// A link and the frames waiting on it.
///
/// FFmpeg keeps a `FFFrameQueue` inside every `FilterLinkInternal`; here the
/// queue lives alongside the link configuration.
#[derive(Debug)]
pub struct Link {
    pub src_node: NodeId,
    pub src_pad: usize,
    pub dst_node: NodeId,
    pub dst_pad: usize,
    pub config: FilterLinkConfig,
    pub queue: VecDeque<Frame>,
}

impl Link {
    /// Number of frames currently queued on this link.
    pub fn queued_frames(&self) -> usize {
        self.queue.len()
    }
}

/// A filter instance inside a [`Graph`].
pub struct Node {
    /// Unique instance name, matching FFmpeg's `AVFilterContext.name`.
    pub instance_name: String,
    /// Name of the registered filter this instance was created from.
    pub filter_name: String,
    pub filter: Box<dyn Filter>,
    pub input_links: Vec<LinkId>,
    pub output_links: Vec<LinkId>,
    /// Frames injected at a source node (it has no input pads to receive them).
    pending: VecDeque<Frame>,
}

/// A directed filter graph built from registered filters.
///
/// Lifecycle mirrors FFmpeg:
/// - `add_*` builds the graph (`avfilter_graph_alloc`/`avfilter_graph_create_filter`/`avfilter_link`)
/// - [`Graph::config`] validates pads and activates filters (`avfilter_graph_config`)
/// - [`Graph::push`] feeds a source (`av_buffersrc_add_frame`)
/// - [`Graph::run`] pumps queued frames (`ff_filter_graph_run_once`)
/// - [`Graph::pull`] collects sink output (`av_buffersink_get_frame`)
pub struct Graph {
    nodes: Vec<Node>,
    links: Vec<Link>,
    /// Frames produced by nodes with no output links (graph sinks).
    output_queue: VecDeque<Frame>,
    active: bool,
    eof: bool,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            links: Vec::new(),
            output_queue: VecDeque::new(),
            active: false,
            eof: false,
        }
    }

    /// Number of registered filters visible to the node registry.
    ///
    /// Equivalent to iterating FFmpeg's filter registry.
    pub fn nb_registered_filters() -> usize {
        filters().count()
    }

    /// Add an instance of a caller-supplied filter.
    ///
    /// FFmpeg requires every filter instance in a graph to have a unique
    /// name; enforce the same rule here.
    pub fn add_node(
        &mut self,
        instance_name: impl Into<String>,
        filter: Box<dyn Filter>,
    ) -> Result<NodeId> {
        let instance_name = instance_name.into();
        if self.node_id_by_name(&instance_name).is_some() {
            return Err(Error::Other(format!(
                "filter instance '{instance_name}' already exists in the graph"
            )));
        }
        let filter_name = filter.descriptor().name.to_owned();
        let id = self.nodes.len();
        self.nodes.push(Node {
            instance_name,
            filter_name,
            filter,
            input_links: Vec::new(),
            output_links: Vec::new(),
            pending: VecDeque::new(),
        });
        Ok(id)
    }

    /// Add an instance of a filter looked up by its registered name.
    ///
    /// This is the node-registry path: `find_filter_by_name` maps a name to a
    /// `FilterFactory`, whose `create()` builds the instance.
    pub fn add_node_by_name(
        &mut self,
        instance_name: impl Into<String>,
        filter_name: &str,
    ) -> Result<NodeId> {
        let factory = find_filter_by_name(filter_name)
            .ok_or_else(|| Error::Other(format!("no such filter: '{filter_name}'")))?;
        self.add_node(instance_name, factory.create()?)
    }

    /// Look up a node by its instance name.
    pub fn node_id_by_name(&self, instance_name: &str) -> Option<NodeId> {
        self.nodes
            .iter()
            .position(|n| n.instance_name == instance_name)
    }

    /// Connect output pad `src_pad` of `src` to input pad `dst_pad` of `dst`.
    ///
    /// Validation follows FFmpeg's `avfilter_link` (libavfilter/avfilter.c:149):
    /// pad indices must be in range, a pad may only be linked once, media
    /// types must match, and the graph must stay acyclic.
    pub fn link(
        &mut self,
        src: NodeId,
        src_pad: usize,
        dst: NodeId,
        dst_pad: usize,
        config: FilterLinkConfig,
    ) -> Result<LinkId> {
        let Some(src_desc) = self.nodes.get(src).map(|n| n.filter.descriptor()) else {
            return Err(Error::Other(format!("no such source node: {src}")));
        };
        let Some(dst_desc) = self.nodes.get(dst).map(|n| n.filter.descriptor()) else {
            return Err(Error::Other(format!("no such destination node: {dst}")));
        };

        // FFmpeg: if (src->nb_outputs <= srcpad || dst->nb_inputs <= dstpad ||
        //             src->outputs[srcpad] || dst->inputs[dstpad])
        //             return AVERROR(EINVAL);
        if src_pad >= src_desc.outputs.len() {
            return Err(Error::Other(format!(
                "output pad {src_pad} out of range for filter '{}'",
                src_desc.name
            )));
        }
        if dst_pad >= dst_desc.inputs.len() {
            return Err(Error::Other(format!(
                "input pad {dst_pad} out of range for filter '{}'",
                dst_desc.name
            )));
        }
        if self
            .links
            .iter()
            .any(|l| l.src_node == src && l.src_pad == src_pad)
        {
            return Err(Error::Other(format!(
                "output pad {src_pad} of '{}' is already linked",
                self.nodes[src].instance_name
            )));
        }
        if self
            .links
            .iter()
            .any(|l| l.dst_node == dst && l.dst_pad == dst_pad)
        {
            return Err(Error::Other(format!(
                "input pad {dst_pad} of '{}' is already linked",
                self.nodes[dst].instance_name
            )));
        }

        // FFmpeg: if (src->output_pads[srcpad].type != dst->input_pads[dstpad].type)
        //             return AVERROR(EINVAL);
        let src_type = src_desc.outputs[src_pad].media_type;
        let dst_type = dst_desc.inputs[dst_pad].media_type;
        if src_type != dst_type {
            return Err(Error::Other(format!(
                "media type mismatch between '{}' output pad {src_pad} ({src_type}) and '{}' input pad {dst_pad} ({dst_type})",
                self.nodes[src].instance_name, self.nodes[dst].instance_name
            )));
        }
        if config.media_type != src_type {
            return Err(Error::Other(format!(
                "link media type ({}) does not match output pad type ({src_type})",
                config.media_type
            )));
        }

        // A cycle would never drain with this graph's pull-based run loop.
        if src == dst || self.reaches(dst, src) {
            return Err(Error::Other(
                "filter graph must not contain cycles".to_owned(),
            ));
        }

        let id = self.links.len();
        self.links.push(Link {
            src_node: src,
            src_pad,
            dst_node: dst,
            dst_pad,
            config,
            queue: VecDeque::new(),
        });
        self.nodes[src].output_links.push(id);
        self.nodes[dst].input_links.push(id);
        Ok(id)
    }

    /// Convenience wrapper around [`Graph::link`] using instance names and
    /// pad indices (FFmpeg's graph parser produces exactly this kind of spec).
    pub fn link_by_names(
        &mut self,
        src: &str,
        src_pad: usize,
        dst: &str,
        dst_pad: usize,
        config: FilterLinkConfig,
    ) -> Result<LinkId> {
        let src_id = self
            .node_id_by_name(src)
            .ok_or_else(|| Error::Other(format!("no such filter instance: '{src}'")))?;
        let dst_id = self
            .node_id_by_name(dst)
            .ok_or_else(|| Error::Other(format!("no such filter instance: '{dst}'")))?;
        self.link(src_id, src_pad, dst_id, dst_pad, config)
    }

    /// True when `from` can reach `target` by following existing links.
    fn reaches(&self, from: NodeId, target: NodeId) -> bool {
        let mut stack = vec![from];
        let mut seen = HashSet::new();
        while let Some(node) = stack.pop() {
            if node == target {
                return true;
            }
            if !seen.insert(node) {
                continue;
            }
            for &link_id in &self.nodes[node].output_links {
                stack.push(self.links[link_id].dst_node);
            }
        }
        false
    }

    /// Check that every pad of every node is connected.
    ///
    /// Direct port of `graph_check_validity` (libavfilter/avfiltergraph.c:232).
    pub fn validate(&self) -> Result<()> {
        for node in &self.nodes {
            let desc = node.filter.descriptor();
            for (pad, expected) in desc.inputs.iter().enumerate() {
                if !self
                    .links
                    .iter()
                    .any(|l| l.dst_node == self.node_index(node) && l.dst_pad == pad)
                {
                    return Err(Error::Other(format!(
                        "input pad \"{}\" with type {} of filter instance \"{}\" not connected to any source",
                        expected.name, expected.media_type, node.instance_name
                    )));
                }
            }
            for (pad, expected) in desc.outputs.iter().enumerate() {
                if !self
                    .links
                    .iter()
                    .any(|l| l.src_node == self.node_index(node) && l.src_pad == pad)
                {
                    return Err(Error::Other(format!(
                        "output pad \"{}\" with type {} of filter instance \"{}\" not connected to any destination",
                        expected.name, expected.media_type, node.instance_name
                    )));
                }
            }
        }
        Ok(())
    }

    /// Helper for `validate`: index of a node inside `self.nodes`.
    fn node_index(&self, node: &Node) -> NodeId {
        self.nodes
            .iter()
            .position(|n| std::ptr::eq(n, node))
            .expect("node belongs to this graph")
    }

    /// Validate the graph and activate all filters (FFmpeg's
    /// `avfilter_graph_config`).
    pub fn config(&mut self) -> Result<()> {
        self.validate()?;
        self.activate()
    }

    /// Activate every filter instance once.
    pub fn activate(&mut self) -> Result<()> {
        for node in &mut self.nodes {
            node.filter.activate()?;
        }
        self.active = true;
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn nb_nodes(&self) -> usize {
        self.nodes.len()
    }

    pub fn nb_links(&self) -> usize {
        self.links.len()
    }

    /// Read-only view of the nodes, in insertion order.
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Read-only view of the links, in creation order.
    pub fn links(&self) -> &[Link] {
        &self.links
    }

    pub fn link_queued_frames(&self, link_id: LinkId) -> Result<usize> {
        self.links
            .get(link_id)
            .map(Link::queued_frames)
            .ok_or(Error::InvalidArgument)
    }

    /// Number of frames waiting to be collected at the graph sinks.
    pub fn output_len(&self) -> usize {
        self.output_queue.len()
    }

    /// Inject a frame at a source node (a node with no input pads).
    ///
    /// Mirrors `av_buffersrc_add_frame`: the frame is queued at the source and
    /// will be forwarded by the next [`Graph::run`].
    pub fn push(&mut self, node_id: NodeId, frame: Frame) -> Result<()> {
        let node = self.nodes.get_mut(node_id).ok_or(Error::InvalidArgument)?;
        if !node.filter.descriptor().inputs.is_empty() {
            return Err(Error::Other(format!(
                "node '{}' is not a source (it has input pads)",
                node.instance_name
            )));
        }
        let expected = node.filter.descriptor().outputs[0].media_type;
        let actual = media_type_of(&frame);
        if expected != actual {
            return Err(Error::Other(format!(
                "media type mismatch: source '{}' produces {expected}, frame is {actual}",
                node.instance_name
            )));
        }
        node.pending.push_back(frame);
        Ok(())
    }

    /// Find a node with at least one frame ready to process.
    fn find_ready_node(&self) -> Option<NodeId> {
        for (id, node) in self.nodes.iter().enumerate() {
            if node.filter.descriptor().inputs.is_empty() {
                if !node.pending.is_empty() {
                    return Some(id);
                }
            } else if node
                .input_links
                .iter()
                .any(|&link_id| !self.links[link_id].queue.is_empty())
            {
                return Some(id);
            }
        }
        None
    }

    /// Take one frame from the first ready input of `node_id`.
    fn take_input_frame(&mut self, node_id: NodeId) -> Option<Frame> {
        if self.nodes[node_id].filter.descriptor().inputs.is_empty() {
            return self.nodes[node_id].pending.pop_front();
        }
        let link_id = self.nodes[node_id]
            .input_links
            .iter()
            .find(|&&link_id| !self.links[link_id].queue.is_empty())
            .copied()?;
        self.links[link_id].queue.pop_front()
    }

    /// Route the frames returned by `filter_frame` downstream.
    ///
    /// `Filter::filter_frame` returns an unlabelled `Vec<Frame>`, so when a
    /// node has several output links the frames are distributed round-robin;
    /// a node with no output links is a graph sink and its frames are queued
    /// for [`Graph::pull`].
    fn route_outputs(&mut self, node_id: NodeId, frames: Vec<Frame>) {
        let output_links = self.nodes[node_id].output_links.clone();
        if output_links.is_empty() {
            self.output_queue.extend(frames);
            return;
        }
        for (i, frame) in frames.into_iter().enumerate() {
            let link_id = output_links[i % output_links.len()];
            self.links[link_id].queue.push_back(frame);
        }
    }

    /// Pump all queued frames through the graph until no node can make
    /// progress. Returns the number of `filter_frame` calls made.
    pub fn run(&mut self) -> Result<usize> {
        if !self.active {
            self.activate()?;
        }
        let mut processed = 0usize;
        while let Some(node_id) = self.find_ready_node() {
            let Some(frame) = self.take_input_frame(node_id) else {
                continue;
            };
            let outputs = self.nodes[node_id].filter.filter_frame(frame)?;
            self.route_outputs(node_id, outputs);
            processed += 1;
            if processed >= MAX_RUN_STEPS {
                return Err(Error::Other(format!(
                    "filter graph exceeded {MAX_RUN_STEPS} processing steps"
                )));
            }
        }
        Ok(processed)
    }

    /// Inject one frame at the graph's single source, run, and return all
    /// frames that reached a sink.
    ///
    /// Convenience for the common linear-chain case.
    pub fn process(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let sources: Vec<NodeId> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.filter.descriptor().inputs.is_empty())
            .map(|(id, _)| id)
            .collect();
        if sources.len() != 1 {
            return Err(Error::Other(format!(
                "process() requires exactly one source node, found {}",
                sources.len()
            )));
        }
        self.push(sources[0], frame)?;
        self.run()?;
        Ok(self.drain())
    }

    /// Take the next frame produced by a graph sink.
    ///
    /// `Error::Again` means no frame is available yet (FFmpeg's
    /// `av_buffersink_get_frame` returns `EAGAIN`); after [`Graph::close`]
    /// it returns `Error::Eof` instead.
    pub fn pull(&mut self) -> Result<Frame> {
        match self.output_queue.pop_front() {
            Some(frame) => Ok(frame),
            None if self.eof => Err(Error::Eof),
            None => Err(Error::Again),
        }
    }

    /// Remove and return every frame currently queued at the graph sinks.
    pub fn drain(&mut self) -> Vec<Frame> {
        self.output_queue.drain(..).collect()
    }

    /// Signal end of input: run once more to flush queued frames, then
    /// [`Graph::pull`] reports `Error::Eof` when the output queue is empty.
    pub fn close(&mut self) -> Result<()> {
        self.run()?;
        self.eof = true;
        Ok(())
    }

    pub fn is_eof(&self) -> bool {
        self.eof
    }
}

/// Media type of a frame, matching the frame data variant.
fn media_type_of(frame: &Frame) -> MediaType {
    if frame.is_video() {
        MediaType::Video
    } else {
        MediaType::Audio
    }
}

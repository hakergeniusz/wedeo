use wedeo_core::error::{Error, Result};
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_core::rational::Rational;

use crate::Filter;
use crate::formats::{FilterFormats, FormatsConfig};
use crate::registry::find_filter_by_name;

/// Configuration for a link between two filter nodes.
#[derive(Debug, Clone)]
pub struct FilterLinkConfig {
    pub media_type: MediaType,
    pub time_base: Rational,
}

/// A link between two filter nodes.
#[derive(Debug)]
pub struct FilterLink {
    pub src_node: usize,
    pub src_pad: usize,
    pub dst_node: usize,
    pub dst_pad: usize,
    pub config: FilterLinkConfig,
}

/// A node in the filter graph.
pub struct FilterNode {
    pub name: String,
    pub filter: Box<dyn Filter>,
    pub input_links: Vec<usize>,
    pub output_links: Vec<usize>,
}

/// The filter graph — manages filter nodes and links.
pub struct FilterGraph {
    nodes: Vec<FilterNode>,
    links: Vec<FilterLink>,
    /// Per-node format configuration produced by [`Filter::query_formats`].
    node_formats: Vec<FormatsConfig>,
    /// Format negotiated on each link, parallel to `links`.
    link_formats: Vec<FilterFormats>,
}

impl FilterGraph {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            links: Vec::new(),
            node_formats: Vec::new(),
            link_formats: Vec::new(),
        }
    }

    pub fn add_node(&mut self, name: String, filter: Box<dyn Filter>) -> usize {
        let index = self.nodes.len();
        let formats = FormatsConfig::from_descriptor(filter.descriptor());
        self.nodes.push(FilterNode {
            name,
            filter,
            input_links: Vec::new(),
            output_links: Vec::new(),
        });
        self.node_formats.push(formats);
        index
    }

    /// Add a node from a registered filter name, passing options to its factory.
    ///
    /// Factories that do not override
    /// [`crate::registry::FilterFactory::create_with_options`] ignore the
    /// options, so all existing registrations work unchanged.
    pub fn add_node_with_options(
        &mut self,
        name: String,
        filter_name: &str,
        options: &[(&str, &str)],
    ) -> Result<usize> {
        let factory = find_filter_by_name(filter_name).ok_or(Error::FilterNotFound)?;
        let filter = factory.create_with_options(options)?;
        Ok(self.add_node(name, filter))
    }

    /// Add a node from a registered filter name without options.
    pub fn add_node_by_name(&mut self, name: String, filter_name: &str) -> Result<usize> {
        self.add_node_with_options(name, filter_name, &[])
    }

    pub fn add_link(
        &mut self,
        src_node: usize,
        src_pad: usize,
        dst_node: usize,
        dst_pad: usize,
        config: FilterLinkConfig,
    ) -> usize {
        let index = self.links.len();
        self.links.push(FilterLink {
            src_node,
            src_pad,
            dst_node,
            dst_pad,
            config,
        });
        self.link_formats.push(FilterFormats::Any);
        self.nodes[src_node].output_links.push(index);
        self.nodes[dst_node].input_links.push(index);
        index
    }

    pub fn nb_nodes(&self) -> usize {
        self.nodes.len()
    }

    pub fn nb_links(&self) -> usize {
        self.links.len()
    }

    /// Run format negotiation over every node and link.
    ///
    /// FFmpeg's `avfilter_graph_config()` calls each filter's `query_formats()`
    /// and then intersects the format lists of linked pads, failing with
    /// `AVERROR(EINVAL)` when a link has no common format. Here each node's
    /// [`Filter::query_formats`] fills a fresh [`FormatsConfig`]; the result is
    /// stored per node and the intersection is stored per link. Both pad
    /// configs of a link are narrowed to the negotiated format.
    pub fn query_formats(&mut self) -> Result<()> {
        for (node, cfg) in self.nodes.iter_mut().zip(self.node_formats.iter_mut()) {
            *cfg = FormatsConfig::from_descriptor(node.filter.descriptor());
            node.filter.query_formats(cfg)?;
        }

        let mut resolved = Vec::with_capacity(self.links.len());
        for (index, link) in self.links.iter().enumerate() {
            let src_cfg = self
                .node_formats
                .get(link.src_node)
                .ok_or(Error::InvalidArgument)?;
            let dst_cfg = self
                .node_formats
                .get(link.dst_node)
                .ok_or(Error::InvalidArgument)?;
            let formats = src_cfg
                .negotiate_with(link.src_pad, dst_cfg, link.dst_pad)
                .map_err(|e| {
                    Error::Other(format!(
                        "link {index} ({}:{} -> {}:{}): {e}",
                        self.nodes[link.src_node].name,
                        link.src_pad,
                        self.nodes[link.dst_node].name,
                        link.dst_pad,
                    ))
                })?;
            resolved.push(formats);
        }

        for (link, formats) in self.links.iter().zip(&resolved) {
            if let Some(pad) = self.node_formats[link.src_node]
                .outputs
                .get_mut(link.src_pad)
            {
                pad.formats = formats.clone();
            }
            if let Some(pad) = self.node_formats[link.dst_node]
                .inputs
                .get_mut(link.dst_pad)
            {
                pad.formats = formats.clone();
            }
        }
        self.link_formats = resolved;
        Ok(())
    }

    /// Formats reported by a node's `query_formats` call.
    pub fn node_formats(&self, node: usize) -> Option<&FormatsConfig> {
        self.node_formats.get(node)
    }

    /// Format negotiated on a link, available after [`Self::query_formats`].
    pub fn link_formats(&self, link: usize) -> Option<&FilterFormats> {
        self.link_formats.get(link)
    }

    /// Flush every node at end of stream and collect the emitted frames in
    /// node order.
    ///
    /// This is the graph-level counterpart of [`Filter::flush`]; filters with
    /// no buffered tail return an empty vec through the default impl.
    pub fn flush(&mut self) -> Result<Vec<Frame>> {
        let mut frames = Vec::new();
        for node in &mut self.nodes {
            frames.extend(node.filter.flush()?);
        }
        Ok(frames)
    }

    /// Flush a single node, returning its tail frames.
    pub fn flush_node(&mut self, node: usize) -> Result<Vec<Frame>> {
        self.nodes
            .get_mut(node)
            .ok_or(Error::InvalidArgument)?
            .filter
            .flush()
    }
}

impl Default for FilterGraph {
    fn default() -> Self {
        Self::new()
    }
}

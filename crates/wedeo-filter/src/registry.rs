use wedeo_core::error::{Error, Result};

use crate::{Filter, FilterDescriptor};

/// Factory for creating filter instances. Implementations register via `inventory`.
pub trait FilterFactory: Send + Sync {
    fn descriptor(&self) -> &FilterDescriptor;
    fn create(&self) -> Result<Box<dyn Filter>>;

    /// Create a filter instance with the given options.
    ///
    /// FFmpeg applies options to a filter context with `av_opt_set()` before
    /// `avfilter_graph_config()`. Here options are plain name/value pairs; a
    /// factory whose filter accepts options overrides this method and parses
    /// them. The default ignores the options and delegates to [`Self::create`]
    /// so existing factories keep working unchanged.
    fn create_with_options(&self, _options: &[(&str, &str)]) -> Result<Box<dyn Filter>> {
        self.create()
    }
}

inventory::collect!(&'static dyn FilterFactory);

/// Find a filter factory by name.
pub fn find_filter_by_name(name: &str) -> Option<&'static dyn FilterFactory> {
    inventory::iter::<&'static dyn FilterFactory>()
        .find(|f| f.descriptor().name == name)
        .copied()
}

/// Iterate over all registered filter factories.
pub fn filters() -> impl Iterator<Item = &'static dyn FilterFactory> {
    inventory::iter::<&'static dyn FilterFactory>().copied()
}

/// Create a registered filter by name without options.
pub fn create_filter_by_name(name: &str) -> Result<Box<dyn Filter>> {
    find_filter_by_name(name)
        .ok_or(Error::FilterNotFound)?
        .create()
}

/// Create a registered filter by name, passing options to its factory.
///
/// Factories that do not override
/// [`FilterFactory::create_with_options`] simply ignore the options.
pub fn create_filter_with_options(name: &str, options: &[(&str, &str)]) -> Result<Box<dyn Filter>> {
    find_filter_by_name(name)
        .ok_or(Error::FilterNotFound)?
        .create_with_options(options)
}

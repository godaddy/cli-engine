use std::{path::Path, sync::Arc};

use schemars::JsonSchema;

use crate::{
    GuideEntry, HumanViewDef, Middleware, OutputSchema, RuntimeGroupSpec, SchemaRegistry,
    parse_guides_from_markdown,
};

/// Function used by closure-based modules to register a runtime command group.
pub type ModuleRegister = Arc<dyn Fn(&mut ModuleContext<'_>) -> RuntimeGroupSpec + Send + Sync>;

/// Trait-based module API for larger command domains.
///
/// Implement this when a module has dependencies or enough setup logic that a
/// named type is clearer than a closure.
pub trait CommandModule: Send + Sync + std::fmt::Debug + 'static {
    /// Help category used in root command long help.
    fn category(&self) -> String;

    /// Guide entries contributed by this module.
    fn guides(&self) -> Vec<GuideEntry> {
        Vec::new()
    }

    /// Human views contributed by this module.
    fn views(&self) -> Vec<HumanViewDef> {
        Vec::new()
    }

    /// Registers the module's top-level runtime group.
    fn register(&self, context: &mut ModuleContext<'_>) -> RuntimeGroupSpec;
}

/// Domain-bounded unit of CLI functionality.
///
/// A module usually maps to a product, platform, resource family, or team
/// ownership boundary. It contributes one top-level group plus optional guides
/// and human output views.
#[derive(Clone)]
pub struct Module {
    /// Root help category.
    pub category: String,
    /// Guide entries merged into the CLI-wide guide command.
    pub guides: Vec<GuideEntry>,
    /// Human output views registered before command execution.
    pub views: Vec<HumanViewDef>,
    /// Registration function that returns the module's runtime group.
    pub register: ModuleRegister,
}

impl Module {
    /// Creates a closure-based module.
    #[must_use]
    pub fn new<F>(category: impl Into<String>, register: F) -> Self
    where
        F: Fn(&mut ModuleContext<'_>) -> RuntimeGroupSpec + Send + Sync + 'static,
    {
        Self {
            category: category.into(),
            guides: Vec::new(),
            views: Vec::new(),
            register: Arc::new(register),
        }
    }

    /// Converts a trait-based module into the runtime module type.
    #[must_use]
    pub fn from_command_module<M>(module: M) -> Self
    where
        M: CommandModule,
    {
        let category = module.category();
        let guides = module.guides();
        let views = module.views();
        let module = Arc::new(module);
        Self {
            category,
            guides,
            views,
            register: Arc::new(move |context| module.register(context)),
        }
    }

    /// Adds one guide entry.
    #[must_use]
    pub fn with_guide(mut self, guide: GuideEntry) -> Self {
        self.guides.push(guide);
        self
    }

    /// Adds several guide entries.
    #[must_use]
    pub fn with_guides(mut self, guides: impl IntoIterator<Item = GuideEntry>) -> Self {
        self.guides.extend(guides);
        self
    }

    /// Parses markdown guide entries from embedded `(path, bytes)` pairs.
    #[must_use]
    pub fn with_guides_from_markdown(
        self,
        files: impl IntoIterator<Item = (impl AsRef<Path>, impl AsRef<[u8]>)>,
    ) -> Self {
        self.with_guides(parse_guides_from_markdown(files))
    }

    /// Adds one human output view.
    #[must_use]
    pub fn with_view(mut self, view: HumanViewDef) -> Self {
        self.views.push(view);
        self
    }
}

impl std::fmt::Debug for Module {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Module")
            .field("category", &self.category)
            .field("guides", &self.guides)
            .field("views", &self.views)
            .finish_non_exhaustive()
    }
}

/// Context available while a module registers itself.
///
/// The context gives module code access to shared registries without exposing
/// parser internals. This keeps module registration declarative and easy to
/// copy for new teams.
#[derive(Debug)]
pub struct ModuleContext<'middleware> {
    middleware: &'middleware mut Middleware,
    guides: Vec<GuideEntry>,
    views: Vec<HumanViewDef>,
}

impl<'middleware> ModuleContext<'middleware> {
    pub(crate) fn new(middleware: &'middleware mut Middleware) -> Self {
        Self {
            middleware,
            guides: Vec::new(),
            views: Vec::new(),
        }
    }

    /// Returns a shared view of middleware while registering the module.
    pub fn middleware(&self) -> &Middleware {
        self.middleware
    }

    /// Returns mutable middleware for module-specific setup.
    pub fn middleware_mut(&mut self) -> &mut Middleware {
        self.middleware
    }

    /// Returns the loaded per-application config file for registration-time use.
    ///
    /// Read a consumer-owned section with
    /// [`ConfigFile::section`](crate::config::ConfigFile::section).
    pub fn config(&self) -> &crate::config::ConfigFile {
        &self.middleware.config
    }

    /// Returns the schema registry for direct registration.
    pub fn schema_registry(&mut self) -> &mut SchemaRegistry {
        &mut self.middleware.schema_registry
    }

    /// Registers a compact framework schema for a command path.
    pub fn register_schema<T: OutputSchema>(&mut self, command_path: impl Into<String>) {
        self.middleware
            .schema_registry
            .register::<T>(command_path.into());
    }

    /// Registers JSON Schema generated with `schemars` for a command path.
    pub fn register_json_schema<T: JsonSchema>(&mut self, command_path: impl Into<String>) {
        self.middleware
            .schema_registry
            .register_json_schema::<T>(command_path.into());
    }

    /// Registers a human output view and keeps it with the module.
    pub fn register_view(&mut self, view: HumanViewDef) {
        self.middleware.human_views.register(view.clone());
        self.views.push(view);
    }

    /// Adds one guide entry.
    pub fn add_guide(&mut self, guide: GuideEntry) {
        self.guides.push(guide);
    }

    /// Adds several guide entries.
    pub fn add_guides(&mut self, guides: impl IntoIterator<Item = GuideEntry>) {
        self.guides.extend(guides);
    }

    /// Parses and adds markdown guides from embedded `(path, bytes)` pairs.
    pub fn add_guides_from_markdown(
        &mut self,
        files: impl IntoIterator<Item = (impl AsRef<Path>, impl AsRef<[u8]>)>,
    ) {
        self.add_guides(parse_guides_from_markdown(files));
    }

    pub(crate) fn into_parts(self) -> (Vec<GuideEntry>, Vec<HumanViewDef>) {
        (self.guides, self.views)
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Magnus Thoäng
//! Swagger 2.0 (OpenAPI 2.0) output plugin for yangest.
//!
//! Converts a single YANG module into a RESTCONF-compatible Swagger 2.0
//! document, matching the output of yanger's `yanger_swagger` plugin.

use std::io::Write;
use std::sync::Arc;

use clap::{Arg, ArgMatches};
use serde_json::{Map, Value, json};

use yangest_core::ast::{BuiltInKeyword, Stmt};
use yangest_core::compiler::{
    CompiledModule, ExpansionCtx, ModuleRegistry, SchemaNode, SchemaNodeKind,
};
use yangest_core::plugin::Plugin;

// ── Public options ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopResource {
    All,
    Root,
    Data,
    Operations,
}

impl Default for TopResource {
    fn default() -> Self {
        TopResource::All
    }
}

#[derive(Debug, Clone)]
pub struct SwaggerOptions {
    pub host: Option<String>,
    /// Base path prepended to all paths (default: "/restconf").
    pub base_path: String,
    /// Override the YANG module revision used as API version.
    pub version: Option<String>,
    pub top_resource: TopResource,
    pub omit_query_params: bool,
    pub omit_body_params: bool,
    pub omit_path_params: bool,
    pub path_filter: Option<String>,
    pub int64_as_string: bool,
    /// HTTP methods to include (default: all).
    pub methods: Vec<String>,
}

impl Default for SwaggerOptions {
    fn default() -> Self {
        SwaggerOptions {
            host: None,
            base_path: "/restconf".to_string(),
            version: None,
            top_resource: TopResource::All,
            omit_query_params: false,
            omit_body_params: false,
            omit_path_params: false,
            path_filter: None,
            int64_as_string: false,
            methods: vec![
                "get".into(),
                "post".into(),
                "put".into(),
                "patch".into(),
                "delete".into(),
            ],
        }
    }
}

// ── Plugin ────────────────────────────────────────────────────────────────────

pub struct SwaggerPlugin {
    pub options: SwaggerOptions,
}

impl Default for SwaggerPlugin {
    fn default() -> Self {
        SwaggerPlugin {
            options: SwaggerOptions::default(),
        }
    }
}

impl SwaggerPlugin {
    pub fn new(options: SwaggerOptions) -> Self {
        SwaggerPlugin { options }
    }
}

impl Plugin for SwaggerPlugin {
    fn name(&self) -> &'static str {
        "swagger"
    }
    fn extension(&self) -> &'static str {
        "json"
    }

    fn cli_args(&self) -> Vec<Arg> {
        vec![
            Arg::new("swagger-host")
                .long("swagger-host")
                .value_name("HOST")
                .help("Override the host field in the Swagger document"),
            Arg::new("swagger-base-path")
                .long("swagger-base-path")
                .value_name("PATH")
                .default_value("/restconf")
                .help("Base path prepended to all RESTCONF paths (default: /restconf)"),
            Arg::new("swagger-version")
                .long("swagger-version")
                .value_name("VERSION")
                .help("Override the API version (defaults to YANG module revision)"),
            Arg::new("swagger-top-resource")
                .long("swagger-top-resource")
                .value_name("RESOURCE")
                .default_value("all")
                .help(
                    "Top-level RESTCONF resources to emit: \
                     all (default), root, data, or operations",
                ),
            Arg::new("swagger-omit-query-params")
                .long("swagger-omit-query-params")
                .action(clap::ArgAction::SetTrue)
                .help("Omit RESTCONF query parameters from all operations"),
            Arg::new("swagger-omit-body-params")
                .long("swagger-omit-body-params")
                .action(clap::ArgAction::SetTrue)
                .help("Omit request body parameters from all operations"),
            Arg::new("swagger-omit-path-params")
                .long("swagger-omit-path-params")
                .action(clap::ArgAction::SetTrue)
                .help("Omit path parameters from all operations"),
            Arg::new("swagger-path-filter")
                .long("swagger-path-filter")
                .value_name("FILTER")
                .help("Only emit paths whose URL contains FILTER as a substring"),
            Arg::new("swagger-int64-as-string")
                .long("swagger-int64-as-string")
                .action(clap::ArgAction::SetTrue)
                .help("Encode int64/uint64 values as JSON strings instead of integers"),
            Arg::new("swagger-methods")
                .long("swagger-methods")
                .value_name("METHODS")
                .value_delimiter(',')
                .help(
                    "Comma-separated list of HTTP methods to include \
                     (default: get,post,put,patch,delete,options)",
                ),
        ]
    }

    fn configure(&mut self, matches: &ArgMatches) {
        self.options.host = matches.get_one::<String>("swagger-host").cloned();
        if let Some(bp) = matches.get_one::<String>("swagger-base-path") {
            self.options.base_path = bp.clone();
        }
        self.options.version = matches.get_one::<String>("swagger-version").cloned();
        self.options.top_resource = match matches
            .get_one::<String>("swagger-top-resource")
            .map(String::as_str)
        {
            Some("root") => TopResource::Root,
            Some("data") => TopResource::Data,
            Some("operations") => TopResource::Operations,
            _ => TopResource::All,
        };
        self.options.omit_query_params = matches.get_flag("swagger-omit-query-params");
        self.options.omit_body_params = matches.get_flag("swagger-omit-body-params");
        self.options.omit_path_params = matches.get_flag("swagger-omit-path-params");
        self.options.path_filter = matches.get_one::<String>("swagger-path-filter").cloned();
        self.options.int64_as_string = matches.get_flag("swagger-int64-as-string");
        let methods: Vec<String> = matches
            .get_many::<String>("swagger-methods")
            .map(|v| v.cloned().collect())
            .unwrap_or_default();
        if !methods.is_empty() {
            self.options.methods = methods;
        }
    }

    /// Swagger produces one document for one data module.
    /// When multiple modules are passed, picks the first with data children
    /// and warns. Use `--output-dir` for proper batch mode.
    fn emit(
        &self,
        modules: &[Arc<CompiledModule>],
        _registry: &ModuleRegistry,
        ctx: &ExpansionCtx<'_>,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        let module = modules
            .iter()
            .find(|m| !m.children(ctx).is_empty())
            .or_else(|| modules.first());

        let module = match module {
            Some(m) => m,
            None => {
                eprintln!("swagger: no module to emit");
                return Ok(());
            }
        };

        if modules.len() > 1 {
            eprintln!(
                "swagger: multiple modules provided; using '{}'. \
                 Use --output-dir for batch mode.",
                module.key.name
            );
        }

        let doc = build_swagger(module, &self.options, ctx);
        let json = serde_json::to_string_pretty(&doc)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        writeln!(out, "{}", json)
    }
}

// ── Top-level document builder ────────────────────────────────────────────────

fn build_swagger<'a>(
    module: &'a CompiledModule,
    opts: &'a SwaggerOptions,
    expansion_ctx: &'a ExpansionCtx<'a>,
) -> Value {
    let title = module.key.name.clone();
    let description = stmt_description(&module.stmt).unwrap_or_default();
    let version = opts
        .version
        .clone()
        .or_else(|| module.key.revision.clone())
        .unwrap_or_else(|| "1.0.0".to_string());

    let mut ctx = BuildCtx {
        opts,
        module,
        expansion_ctx,
        paths: Map::new(),
        definitions: Map::new(),
        parameters: Map::new(),
    };

    // Standard shared parameters (RESTCONF query params)
    if !opts.omit_query_params {
        add_query_params(&mut ctx.parameters);
    }
    // Standard responses
    let mut responses = Map::new();
    add_standard_responses(&mut responses);

    // root resource  /restconf
    if matches!(opts.top_resource, TopResource::All | TopResource::Root) {
        emit_root_path(&mut ctx);
        emit_yang_library_version_path(&mut ctx);
    }

    let top_children = module.children(expansion_ctx);

    // /restconf/operations
    if matches!(
        opts.top_resource,
        TopResource::All | TopResource::Operations
    ) {
        emit_operations_list_path(&mut ctx);
        let rpcs: Vec<SchemaNode> = top_children
            .iter()
            .filter(|n| matches!(n.kind, SchemaNodeKind::Rpc { .. }))
            .cloned()
            .collect();
        for rpc in &rpcs {
            emit_rpc_path(&mut ctx, rpc);
        }
    }

    // /restconf/data
    if matches!(opts.top_resource, TopResource::All | TopResource::Data) {
        emit_data_top_path(&mut ctx);
        let data_nodes: Vec<SchemaNode> = top_children
            .iter()
            .filter(|n| {
                !matches!(
                    n.kind,
                    SchemaNodeKind::Rpc { .. } | SchemaNodeKind::Notification { .. }
                )
            })
            .cloned()
            .collect();
        emit_children_paths(&mut ctx, &data_nodes, "/data", true);
    }

    let tags = build_tags(opts);

    let mut doc = json!({
        "swagger": "2.0",
        "info": {
            "title": title,
            "description": description,
            "version": version
        },
        "basePath": opts.base_path,
        "tags": tags,
        "schemes": ["http", "https"],
        "produces": ["application/yang-data+json"],
        "consumes": ["application/yang-data+json"],
        "paths": ctx.paths,
        "parameters": ctx.parameters,
        "responses": responses,
        "securityDefinitions": {
            "basicAuth": { "type": "basic" }
        },
        "definitions": ctx.definitions
    });

    if let Some(ref h) = opts.host {
        doc["host"] = json!(h);
    }

    doc
}

// ── Build context ─────────────────────────────────────────────────────────────

struct BuildCtx<'a> {
    opts: &'a SwaggerOptions,
    module: &'a CompiledModule,
    expansion_ctx: &'a ExpansionCtx<'a>,
    paths: Map<String, Value>,
    definitions: Map<String, Value>,
    parameters: Map<String, Value>,
}

// ── Path emission ─────────────────────────────────────────────────────────────

fn emit_root_path(ctx: &mut BuildCtx<'_>) {
    let path = "/";
    let methods = root_methods(ctx);
    if !methods.is_empty() && matches_filter(path, ctx.opts) {
        ctx.paths.insert(path.to_string(), json!(methods));
    }
}

fn emit_yang_library_version_path(ctx: &mut BuildCtx<'_>) {
    let path = "/yang-library-version";
    if !matches_filter(path, ctx.opts) {
        return;
    }
    let mut m = Map::new();
    for method in &["get", "head"] {
        if method_allowed(method, ctx.opts) {
            m.insert(
                (*method).into(),
                make_method(ctx, method, path, "yang-library-version", &[], &[]),
            );
        }
    }
    if !m.is_empty() {
        ctx.paths.insert(path.to_string(), json!(m));
    }
}

fn emit_data_top_path(ctx: &mut BuildCtx<'_>) {
    let path = "/data";
    if matches_filter(path, ctx.opts) {
        let methods = data_top_methods(ctx, path);
        ctx.paths.insert(path.to_string(), json!(methods));
    }
}

fn emit_operations_list_path(ctx: &mut BuildCtx<'_>) {
    let path = "/operations";
    if matches_filter(path, ctx.opts) {
        let methods = operations_list_methods(ctx, path);
        ctx.paths.insert(path.to_string(), json!(methods));
    }
}

fn emit_rpc_path(ctx: &mut BuildCtx<'_>, node: &SchemaNode) {
    let node_name = node_path_name(node, ctx.module, true);
    let path = format!("/operations/{}", node_name);
    if matches_filter(&path, ctx.opts) {
        let methods = rpc_methods(ctx, node, &path);
        ctx.paths.insert(path, json!(methods));
    }
}

fn emit_children_paths(
    ctx: &mut BuildCtx<'_>,
    nodes: &[SchemaNode],
    parent_path: &str,
    is_top: bool,
) {
    for node in nodes {
        emit_node_path(ctx, node, parent_path, is_top);
    }
}

fn emit_node_path(ctx: &mut BuildCtx<'_>, node: &SchemaNode, parent_path: &str, is_top: bool) {
    match &node.kind {
        SchemaNodeKind::Choice { .. } => {
            for case in node.children(ctx.expansion_ctx) {
                emit_children_paths(ctx, &case.children(ctx.expansion_ctx), parent_path, is_top);
            }
            return;
        }
        SchemaNodeKind::Rpc { .. } | SchemaNodeKind::Notification { .. } => return,
        _ => {}
    }

    let node_name = node_path_name(node, ctx.module, is_top);
    let base_path = format!("{}/{}", parent_path, node_name);

    if matches_filter(&base_path, ctx.opts) {
        let methods = node_base_methods(ctx, node, &base_path, "data");
        if !methods.is_empty() {
            ctx.paths.insert(base_path.clone(), json!(methods));
        }
    }

    let child_path = node_child_path(node, &base_path);
    if let Some(ref cp) = child_path {
        if matches_filter(cp, ctx.opts) {
            let methods = node_child_methods(ctx, node, cp, "data");
            if !methods.is_empty() {
                ctx.paths.insert(cp.clone(), json!(methods));
            }
        }
    }

    let next_path = child_path.as_deref().unwrap_or(&base_path);
    match &node.kind {
        SchemaNodeKind::Container { .. } => {
            let children = node.children(ctx.expansion_ctx);
            emit_children_paths(ctx, &children, &base_path, false);
        }
        SchemaNodeKind::List { .. } => {
            let children = node.children(ctx.expansion_ctx);
            let child_nodes: Vec<SchemaNode> = children
                .iter()
                .filter(|c| {
                    !matches!(
                        c.kind,
                        SchemaNodeKind::Rpc { .. } | SchemaNodeKind::Notification { .. }
                    )
                })
                .cloned()
                .collect();
            emit_children_paths(ctx, &child_nodes, next_path, false);
            let actions: Vec<SchemaNode> = children
                .iter()
                .filter(|c| matches!(c.kind, SchemaNodeKind::Action { .. }))
                .cloned()
                .collect();
            for action in &actions {
                emit_action_path(ctx, action, next_path);
            }
        }
        _ => {}
    }
}

fn emit_action_path(ctx: &mut BuildCtx<'_>, node: &SchemaNode, parent_path: &str) {
    let node_name = node_path_name(node, ctx.module, false);
    let path = format!("{}/{}", parent_path, node_name);
    if matches_filter(&path, ctx.opts) {
        let methods = rpc_methods(ctx, node, &path);
        ctx.paths.insert(path, json!(methods));
    }
}

// ── Path naming ───────────────────────────────────────────────────────────────

fn node_path_name(node: &SchemaNode, module: &CompiledModule, is_top: bool) -> String {
    if is_top {
        // Top-level nodes use module-prefix:name
        format!("{}:{}", module.key.name, node.name)
    } else if node.module_name != module.key.name {
        // Augmented node from a different module
        format!("{}:{}", node.module_name, node.name)
    } else {
        node.name.clone()
    }
}

fn node_child_path(node: &SchemaNode, base_path: &str) -> Option<String> {
    match &node.kind {
        SchemaNodeKind::List { key, .. } => {
            let keys_str = if key.is_empty() {
                "id".to_string()
            } else {
                key.iter()
                    .map(|k| format!("{{{}-{}}}", node.name, k))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            Some(format!("{}={}", base_path, keys_str))
        }
        SchemaNodeKind::LeafList { .. } => Some(format!("{}={{{}-id}}", base_path, node.name)),
        _ => None,
    }
}

fn matches_filter(path: &str, opts: &SwaggerOptions) -> bool {
    match &opts.path_filter {
        None => true,
        Some(f) if f.is_empty() => false,
        Some(f) => path.contains(f.as_str()),
    }
}

// ── HTTP method builders ──────────────────────────────────────────────────────

fn method_allowed(method: &str, opts: &SwaggerOptions) -> bool {
    opts.methods.iter().any(|m| m == method)
}

fn root_methods(ctx: &mut BuildCtx<'_>) -> Map<String, Value> {
    let mut m = Map::new();
    let path = "/";
    if method_allowed("get", ctx.opts) {
        m.insert(
            "get".into(),
            make_method(ctx, "get", path, "root", &[], &[]),
        );
    }
    if method_allowed("head", ctx.opts) {
        m.insert(
            "head".into(),
            make_method(ctx, "head", path, "root", &[], &[]),
        );
    }
    m
}

fn data_top_methods(ctx: &mut BuildCtx<'_>, path: &str) -> Map<String, Value> {
    let mut m = Map::new();
    for method in &["get", "head", "post", "put", "patch"] {
        if method_allowed(method, ctx.opts) {
            m.insert(
                (*method).into(),
                make_method(ctx, method, path, "data", &[], &[]),
            );
        }
    }
    m
}

fn operations_list_methods(ctx: &mut BuildCtx<'_>, path: &str) -> Map<String, Value> {
    let mut m = Map::new();
    if method_allowed("get", ctx.opts) {
        m.insert(
            "get".into(),
            make_method(ctx, "get", path, "operations", &[], &[]),
        );
    }
    m
}

fn rpc_methods(ctx: &mut BuildCtx<'_>, node: &SchemaNode, path: &str) -> Map<String, Value> {
    let mut m = Map::new();
    if method_allowed("post", ctx.opts) {
        let (input_def, output_def) = rpc_definitions(ctx, node, path);
        let body_params = if ctx.opts.omit_body_params {
            vec![]
        } else if let Some(ref def_name) = input_def {
            vec![json!({"$ref": format!("#/parameters/{}", ref_name(def_name))})]
        } else {
            vec![]
        };
        let response_schema = output_def
            .as_ref()
            .map(|d| json!({"$ref": format!("#/definitions/{}", ref_name(d))}));
        m.insert(
            "post".into(),
            make_rpc_method(ctx, node, path, body_params, response_schema),
        );
    }
    m
}

fn node_base_methods(
    ctx: &mut BuildCtx<'_>,
    node: &SchemaNode,
    path: &str,
    mode: &str,
) -> Map<String, Value> {
    let config = node_is_config(node);
    let is_list_or_leaflist = matches!(
        node.kind,
        SchemaNodeKind::List { .. } | SchemaNodeKind::LeafList { .. }
    );

    let mut m = Map::new();

    if is_list_or_leaflist {
        // Base path for list/leaf-list: OPTIONS only
        if method_allowed("options", ctx.opts) {
            m.insert(
                "options".into(),
                make_method(ctx, "options", path, mode, &[], &[]),
            );
        }
    } else if config {
        // Container, leaf
        for method in &["get", "head", "post", "put", "patch", "delete", "options"] {
            if method_allowed(method, ctx.opts) {
                let path_params = path_params_for_node(ctx, node, path);
                let body_params = body_params_for(ctx, method, node, path, mode);
                m.insert(
                    (*method).into(),
                    make_method(ctx, method, path, mode, &path_params, &body_params),
                );
            }
        }
    } else {
        // State-only: GET, HEAD
        for method in &["get", "head"] {
            if method_allowed(method, ctx.opts) {
                let path_params = path_params_for_node(ctx, node, path);
                m.insert(
                    (*method).into(),
                    make_method(ctx, method, path, mode, &path_params, &[]),
                );
            }
        }
    }
    m
}

fn node_child_methods(
    ctx: &mut BuildCtx<'_>,
    node: &SchemaNode,
    path: &str,
    mode: &str,
) -> Map<String, Value> {
    let config = node_is_config(node);
    let mut m = Map::new();

    let path_params = path_params_for_keyed(ctx, node, path);

    if config {
        for method in &["get", "head", "put", "patch", "delete", "options"] {
            if method_allowed(method, ctx.opts) {
                let body_params = body_params_for(ctx, method, node, path, mode);
                m.insert(
                    (*method).into(),
                    make_method(ctx, method, path, mode, &path_params, &body_params),
                );
            }
        }
    } else {
        for method in &["get", "head"] {
            if method_allowed(method, ctx.opts) {
                m.insert(
                    (*method).into(),
                    make_method(ctx, method, path, mode, &path_params, &[]),
                );
            }
        }
    }
    m
}

// ── Method object builder ─────────────────────────────────────────────────────

fn make_method(
    ctx: &mut BuildCtx<'_>,
    method: &str,
    path: &str,
    mode: &str,
    path_params: &[Value],
    body_params: &[Value],
) -> Value {
    let summary = format!("{} {}", method.to_uppercase(), path);
    let tags = method_tags(method, mode, ctx.opts);
    let mut params: Vec<Value> = path_params.to_vec();
    params.extend_from_slice(body_params);
    if !ctx.opts.omit_query_params {
        params.extend(query_param_refs(method));
    }

    json!({
        "summary": summary,
        "operationId": ref_name(&format!("{}-{}", method, path)),
        "tags": tags,
        "parameters": params,
        "security": [{ "basicAuth": [] }],
        "responses": response_codes(method)
    })
}

fn make_rpc_method(
    ctx: &mut BuildCtx<'_>,
    node: &SchemaNode,
    path: &str,
    body_params: Vec<Value>,
    response_schema: Option<Value>,
) -> Value {
    let tags = method_tags("post", "operations", ctx.opts);
    let desc = node.description.clone().unwrap_or_default();
    let mut responses = json!({
        "201": { "$ref": "#/responses/201" },
        "400": { "$ref": "#/responses/400" },
        "401": { "$ref": "#/responses/401" },
        "404": { "$ref": "#/responses/404" },
        "405": { "$ref": "#/responses/405" },
        "409": { "$ref": "#/responses/409" }
    });
    if let Some(schema) = response_schema {
        responses["200"] = json!({
            "description": "Successful operation",
            "schema": schema
        });
    }

    json!({
        "summary": format!("POST {}", path),
        "description": desc,
        "operationId": ref_name(&format!("post-{}", path)),
        "tags": tags,
        "parameters": body_params,
        "security": [{ "basicAuth": [] }],
        "responses": responses
    })
}

fn method_tags(method: &str, mode: &str, _opts: &SwaggerOptions) -> Vec<Value> {
    vec![json!(mode), json!(method)]
}

// ── RPC/action definitions ────────────────────────────────────────────────────

fn rpc_definitions(
    ctx: &mut BuildCtx<'_>,
    node: &SchemaNode,
    path: &str,
) -> (Option<String>, Option<String>) {
    let input = node.input_children(ctx.expansion_ctx);
    let output = node.output_children(ctx.expansion_ctx);

    let input_def = if !input.is_empty() {
        let def_name = format!("{}-post-input", path);
        let key = ref_name(&def_name);
        let properties = build_properties(ctx, &input, "post");
        let def = json!({
            "type": "object",
            "properties": properties
        });
        ctx.definitions.insert(key.clone(), def);

        let param = json!({
            "name": node.name,
            "in": "body",
            "description": node.description.clone().unwrap_or_default(),
            "required": true,
            "schema": { "$ref": format!("#/definitions/{}", key) }
        });
        ctx.parameters.insert(key.clone(), param);
        Some(key)
    } else {
        None
    };

    let output_def = if !output.is_empty() {
        let def_name = format!("{}-output", path);
        let key = ref_name(&def_name);
        let properties = build_properties(ctx, &output, "get");
        let def = json!({
            "type": "object",
            "properties": properties
        });
        ctx.definitions.insert(key.clone(), def);
        Some(key)
    } else {
        None
    };

    (input_def, output_def)
}

// ── Path & body parameters ────────────────────────────────────────────────────

fn path_params_for_node(ctx: &mut BuildCtx<'_>, node: &SchemaNode, _path: &str) -> Vec<Value> {
    // No path params on the base path of a list (they belong to the child path)
    let _ = (ctx, node);
    vec![]
}

fn path_params_for_keyed(ctx: &mut BuildCtx<'_>, node: &SchemaNode, _path: &str) -> Vec<Value> {
    if ctx.opts.omit_path_params {
        return vec![];
    }
    match &node.kind {
        SchemaNodeKind::List { key, .. } => {
            let children = node.children(ctx.expansion_ctx);
            key.iter()
                .filter_map(|k| {
                    let leaf = children.iter().find(|c| &c.name == k);
                    let type_schema = leaf
                        .and_then(|l| {
                            if let SchemaNodeKind::Leaf { type_stmt, .. } = &l.kind {
                                Some(type_to_swagger(type_stmt, ctx.opts.int64_as_string))
                            } else {
                                None
                            }
                        })
                        .unwrap_or_else(|| json!({"type": "string"}));

                    let param_name = format!("{}-{}", node.name, k);
                    let _key_ref = format!("{{{}}}", param_name);
                    let param = json!({
                        "name": param_name,
                        "in": "path",
                        "description": format!("Key: {}", k),
                        "required": true,
                        "type": type_schema.get("type").and_then(|v| v.as_str()).unwrap_or("string"),
                        "format": type_schema.get("format")
                    });
                    let pname = ref_name(&param_name);
                    ctx.parameters.insert(pname.clone(), param);
                    Some(json!({ "$ref": format!("#/parameters/{}", pname) }))
                })
                .collect()
        }
        SchemaNodeKind::LeafList { type_stmt, .. } => {
            let param_name = format!("{}-id", node.name);
            let type_schema = type_to_swagger(type_stmt, ctx.opts.int64_as_string);
            let param = json!({
                "name": param_name,
                "in": "path",
                "description": format!("Instance identifier for leaf-list {}", node.name),
                "required": true,
                "type": type_schema.get("type").and_then(|v| v.as_str()).unwrap_or("string")
            });
            let pname = ref_name(&param_name);
            ctx.parameters.insert(pname.clone(), param);
            vec![json!({ "$ref": format!("#/parameters/{}", pname) })]
        }
        _ => vec![],
    }
}

fn body_params_for(
    ctx: &mut BuildCtx<'_>,
    method: &str,
    node: &SchemaNode,
    path: &str,
    _mode: &str,
) -> Vec<Value> {
    if ctx.opts.omit_body_params {
        return vec![];
    }
    match method {
        "get" | "head" | "delete" | "options" => return vec![],
        _ => {}
    }
    if method == "post" {
        if let SchemaNodeKind::Leaf { .. } = &node.kind {
            return vec![];
        }
    }

    let def_name = match method {
        "post" => format!("{}-post", path),
        _ => path.to_string(),
    };
    let key = ref_name(&def_name);

    // Build properties for body schema
    let children = node.children(ctx.expansion_ctx);
    let properties = build_properties(ctx, &children, method);

    let body_def = json!({
        "type": "object",
        "properties": properties
    });
    ctx.definitions.insert(key.clone(), body_def);

    let param = json!({
        "name": node.name,
        "in": "body",
        "description": node.description.clone().unwrap_or_default(),
        "required": true,
        "schema": { "$ref": format!("#/definitions/{}", key) }
    });
    ctx.parameters.insert(key.clone(), param);
    vec![json!({ "$ref": format!("#/parameters/{}", key) })]
}

// ── Schema body / properties ──────────────────────────────────────────────────

fn build_properties(ctx: &mut BuildCtx<'_>, nodes: &[SchemaNode], method: &str) -> Value {
    let mut props = Map::new();
    for node in nodes {
        if let Some((name, schema)) = node_schema(ctx, node, method, false) {
            props.insert(name, schema);
        }
    }
    json!(props)
}

fn node_schema(
    ctx: &mut BuildCtx<'_>,
    node: &SchemaNode,
    method: &str,
    _is_top: bool,
) -> Option<(String, Value)> {
    let name = node.name.clone();
    let desc = node.description.clone().unwrap_or_default();

    match &node.kind {
        SchemaNodeKind::Leaf { type_stmt, .. } => {
            let type_info = type_to_swagger(type_stmt, ctx.opts.int64_as_string);
            let mut schema = json!({
                "description": format!("{} (leaf)", desc),
                "x-yang": { "type": "leaf" }
            });
            merge_type(&mut schema, &type_info);
            Some((name, schema))
        }

        SchemaNodeKind::LeafList { type_stmt, .. } => {
            let item_type = type_to_swagger(type_stmt, ctx.opts.int64_as_string);
            let mut items = item_type_obj(&item_type);
            items.insert("description".into(), json!(format!("{} (leaf-list)", desc)));
            let schema = json!({
                "type": "array",
                "x-yang": { "type": "leaf-list" },
                "items": items
            });
            Some((name, schema))
        }

        SchemaNodeKind::Container { presence, .. } => {
            let is_presence = presence.is_some();
            let kind_str = if is_presence {
                "presence"
            } else {
                "non-presence"
            };
            let full_desc = if desc.is_empty() {
                format!("({})", kind_str)
            } else {
                format!("{} ({})", desc, kind_str)
            };
            let children = node.children(ctx.expansion_ctx);
            let props = build_properties(ctx, &children, method);
            let schema = json!({
                "description": full_desc,
                "type": "object",
                "x-yang": { "type": "container", "is_presence": is_presence.to_string() },
                "properties": props
            });
            Some((name, schema))
        }

        SchemaNodeKind::List { .. } => {
            let full_desc = if desc.is_empty() {
                "(list)".to_string()
            } else {
                format!("{} (list)", desc)
            };
            let children = node.children(ctx.expansion_ctx);
            let props = build_properties(ctx, &children, method);
            let schema = json!({
                "type": "array",
                "description": full_desc,
                "x-yang": { "type": "list" },
                "items": {
                    "type": "object",
                    "properties": props
                }
            });
            Some((name, schema))
        }

        SchemaNodeKind::Choice { .. } => {
            let mut merged = Map::new();
            for case in node.children(ctx.expansion_ctx) {
                for child in case.children(ctx.expansion_ctx) {
                    if let Some((n, s)) = node_schema(ctx, &child, method, false) {
                        merged.insert(n, s);
                    }
                }
            }
            // Return each child separately — or skip (choice itself has no schema)
            // We can't return multiple items, so we emit them inline as a synthetic object
            // Yanger flattens choice children into the parent
            for (n, s) in merged {
                // We can only return one — collect all to parent manually
                let _ = (n, s);
            }
            None // caller should handle choice specially
        }

        SchemaNodeKind::AnyXml { .. } | SchemaNodeKind::AnyData { .. } => {
            let kind_str = if matches!(node.kind, SchemaNodeKind::AnyXml { .. }) {
                "anyxml"
            } else {
                "anydata"
            };
            let schema = json!({
                "type": "object",
                "x-yang": { "type": kind_str },
                "description": format!("{} data object", kind_str),
                "properties": {}
            });
            Some((name, schema))
        }

        _ => None,
    }
}

fn item_type_obj(type_info: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    if let Some(t) = type_info.get("type") {
        m.insert("type".into(), t.clone());
    }
    if let Some(f) = type_info.get("format") {
        m.insert("format".into(), f.clone());
    }
    m
}

fn merge_type(schema: &mut Value, type_info: &Value) {
    if let (Value::Object(s), Value::Object(t)) = (schema, type_info) {
        for (k, v) in t {
            s.insert(k.clone(), v.clone());
        }
    }
}

// ── Type mapping ──────────────────────────────────────────────────────────────

fn type_to_swagger(type_stmt: &Stmt, int64_as_string: bool) -> Value {
    let type_name = type_stmt.arg.as_deref().unwrap_or("string");

    // Check for enumeration values
    if type_name == "enumeration" {
        let enums: Vec<Value> = type_stmt
            .substmts
            .iter()
            .filter(|s| {
                matches!(
                    s.keyword,
                    yangest_core::ast::Keyword::BuiltIn(BuiltInKeyword::EnumStmt)
                )
            })
            .filter_map(|s| s.arg.as_ref().map(|a| json!(a)))
            .collect();
        if !enums.is_empty() {
            let first = enums[0].clone();
            return json!({
                "type": "string",
                "format": "enumeration",
                "default": first,
                "enum": enums
            });
        }
        return json!({"type": "string", "format": "enumeration"});
    }

    match type_name {
        "boolean" => json!({"type": "boolean"}),
        "int8" => json!({"type": "integer", "format": "byte"}),
        "int16" => json!({"type": "integer", "format": "int16"}),
        "int32" => json!({"type": "integer", "format": "int32"}),
        "int64" if int64_as_string => json!({"type": "string", "format": "int64"}),
        "int64" => json!({"type": "integer", "format": "int64"}),
        "uint8" => json!({"type": "integer", "format": "byte"}),
        "uint16" => json!({"type": "integer", "format": "uint16"}),
        "uint32" => json!({"type": "integer", "format": "uint32"}),
        "uint64" if int64_as_string => json!({"type": "string", "format": "uint64"}),
        "uint64" => json!({"type": "integer", "format": "uint64"}),
        "decimal64" => json!({"type": "number", "format": "double"}),
        "string" => json!({"type": "string"}),
        "binary" => json!({"type": "string", "format": "binary"}),
        "bits" => json!({"type": "string"}),
        "empty" => json!({"type": "string", "format": "[null]"}),
        "identityref" => json!({"type": "string", "format": "identityref"}),
        "leafref" => json!({"type": "string", "format": "leafref"}),
        "instance-identifier" => json!({"type": "string", "format": "instance-identifier"}),
        "union" => json!({"type": "string", "format": "union"}),
        _ => json!({"type": "string"}),
    }
}

// ── Node config flag ──────────────────────────────────────────────────────────

fn node_is_config(node: &SchemaNode) -> bool {
    node.config.unwrap_or(true)
}

// ── Standard RESTCONF query parameters ───────────────────────────────────────

fn add_query_params(params: &mut Map<String, Value>) {
    let qparams: &[(&str, &str)] = &[
        ("content", "Select config and/or non-config data"),
        ("depth", "Limit the depth of nodes retrieved"),
        ("fields", "Select subset of nodes to retrieve"),
        ("filter", "NETCONF subtree filter"),
        ("with-defaults", "Control retrieval of default values"),
        ("insert", "Insertion point for ordered lists"),
        ("point", "Insertion point reference"),
    ];
    for (name, desc) in qparams {
        params.insert(
            name.to_string(),
            json!({
                "name": name,
                "in": "query",
                "description": desc,
                "required": false,
                "type": "string"
            }),
        );
    }
}

fn query_param_refs(method: &str) -> Vec<Value> {
    match method {
        "get" | "head" => vec![
            json!({"$ref": "#/parameters/content"}),
            json!({"$ref": "#/parameters/depth"}),
            json!({"$ref": "#/parameters/fields"}),
            json!({"$ref": "#/parameters/with-defaults"}),
        ],
        "post" | "put" => vec![
            json!({"$ref": "#/parameters/insert"}),
            json!({"$ref": "#/parameters/point"}),
        ],
        _ => vec![],
    }
}

// ── Standard HTTP responses ───────────────────────────────────────────────────

fn add_standard_responses(responses: &mut Map<String, Value>) {
    let codes: &[(&str, &str)] = &[
        ("200", "OK"),
        ("201", "Created"),
        ("204", "No Content"),
        ("400", "Bad Request"),
        ("401", "Unauthorized"),
        ("404", "Not Found"),
        ("405", "Method Not Allowed"),
        ("409", "Conflict"),
    ];
    for (code, desc) in codes {
        responses.insert(code.to_string(), json!({"description": desc}));
    }
}

fn response_codes(method: &str) -> Value {
    let ok = match method {
        "get" | "head" | "options" => json!({"$ref": "#/responses/200"}),
        "post" | "put" => json!({"$ref": "#/responses/201"}),
        "patch" | "delete" => json!({"$ref": "#/responses/204"}),
        _ => json!({"$ref": "#/responses/200"}),
    };
    json!({
        method_ok_code(method): ok,
        "400": {"$ref": "#/responses/400"},
        "401": {"$ref": "#/responses/401"},
        "404": {"$ref": "#/responses/404"},
        "405": {"$ref": "#/responses/405"},
        "409": {"$ref": "#/responses/409"}
    })
}

fn method_ok_code(method: &str) -> &'static str {
    match method {
        "post" | "put" => "201",
        "patch" | "delete" => "204",
        _ => "200",
    }
}

// ── Tags ──────────────────────────────────────────────────────────────────────

fn build_tags(opts: &SwaggerOptions) -> Value {
    let mut tags = vec![];
    match opts.top_resource {
        TopResource::Root => {
            tags.push(tag_def("root"));
            tags.push(tag_def("yang-library-version"));
        }
        TopResource::Operations => tags.push(tag_def("operations")),
        TopResource::Data => tags.push(tag_def("data")),
        TopResource::All => {
            tags.push(tag_def("root"));
            tags.push(tag_def("yang-library-version"));
            tags.push(tag_def("operations"));
            tags.push(tag_def("data"));
        }
    }
    for m in &opts.methods {
        tags.push(tag_def(m));
    }
    json!(tags)
}

fn tag_def(name: &str) -> Value {
    json!({ "name": name, "description": format!("{} resources", name) })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convert a path or name to a safe $ref key (RFC 6901: ~ → ~0, / → ~1, then URL-encode).
fn ref_name(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

fn stmt_description(stmt: &Stmt) -> Option<String> {
    stmt.get_substmt(BuiltInKeyword::Description)
        .and_then(|s| s.arg.clone())
}

inventory::submit! {
    yangest_core::plugin::PluginRegistration { factory: || Box::new(SwaggerPlugin::default()) }
}

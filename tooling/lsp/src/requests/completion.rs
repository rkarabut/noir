use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::{self, Future},
};

use async_lsp::ResponseError;
use completion_items::{crate_completion_item, simple_completion_item};
use fm::{FileId, PathString};
use kinds::{FunctionCompletionKind, FunctionKind, ModuleCompletionKind, RequestedItems};
use lsp_types::{CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse};
use noirc_errors::{Location, Span};
use noirc_frontend::{
    ast::{
        ArrayLiteral, AsTraitPath, BlockExpression, CallExpression, CastExpression,
        ConstrainStatement, ConstructorExpression, Expression, ForLoopStatement, ForRange,
        FunctionReturnType, Ident, IfExpression, IndexExpression, InfixExpression, LValue, Lambda,
        LetStatement, Literal, MemberAccessExpression, MethodCallExpression, NoirFunction,
        NoirStruct, NoirTrait, NoirTraitImpl, NoirTypeAlias, Path, PathKind, PathSegment, Pattern,
        Statement, TraitImplItem, TraitItem, TypeImpl, UnresolvedGeneric, UnresolvedGenerics,
        UnresolvedType, UseTree, UseTreeKind,
    },
    graph::{CrateId, Dependency},
    hir::{
        def_map::{CrateDefMap, LocalModuleId, ModuleId},
        resolution::path_resolver::{PathResolver, StandardPathResolver},
    },
    macros_api::{ModuleDefId, NodeInterner},
    node_interner::ReferenceId,
    parser::{Item, ItemKind},
    ParsedModule, StructType, Type,
};
use sort_text::underscore_sort_text;

use crate::{utils, LspState};

use super::process_request;

mod builtins;
mod completion_items;
mod kinds;
mod sort_text;
mod tests;

pub(crate) fn on_completion_request(
    state: &mut LspState,
    params: CompletionParams,
) -> impl Future<Output = Result<Option<CompletionResponse>, ResponseError>> {
    let uri = params.text_document_position.clone().text_document.uri;

    let result = process_request(state, params.text_document_position.clone(), |args| {
        let path = PathString::from_path(uri.to_file_path().unwrap());
        args.files.get_file_id(&path).and_then(|file_id| {
            utils::position_to_byte_index(
                args.files,
                file_id,
                &params.text_document_position.position,
            )
            .and_then(|byte_index| {
                let file = args.files.get_file(file_id).unwrap();
                let source = file.source();
                let byte = source.as_bytes().get(byte_index - 1).copied();
                let (parsed_module, _errors) = noirc_frontend::parse_program(source);

                let mut finder = NodeFinder::new(
                    file_id,
                    byte_index,
                    byte,
                    args.crate_id,
                    args.def_maps,
                    args.dependencies,
                    args.interner,
                );
                finder.find(&parsed_module)
            })
        })
    });
    future::ready(result)
}

struct NodeFinder<'a> {
    file: FileId,
    byte_index: usize,
    byte: Option<u8>,
    /// The module ID of the current file.
    root_module_id: ModuleId,
    /// The module ID in scope. This might change as we traverse the AST
    /// if we are analyzing something inside an inline module declaration.
    module_id: ModuleId,
    def_maps: &'a BTreeMap<CrateId, CrateDefMap>,
    dependencies: &'a Vec<Dependency>,
    interner: &'a NodeInterner,
    /// Completion items we find along the way.
    completion_items: Vec<CompletionItem>,
    /// Local variables in the current scope, mapped to their locations.
    /// As we traverse the AST, we collect local variables.
    local_variables: HashMap<String, Span>,
    /// Type parameters in the current scope. These are collected when entering
    /// a struct, a function, etc., and cleared afterwards.
    type_parameters: HashSet<String>,
}

impl<'a> NodeFinder<'a> {
    fn new(
        file: FileId,
        byte_index: usize,
        byte: Option<u8>,
        krate: CrateId,
        def_maps: &'a BTreeMap<CrateId, CrateDefMap>,
        dependencies: &'a Vec<Dependency>,
        interner: &'a NodeInterner,
    ) -> Self {
        // Find the module the current file belongs to
        let def_map = &def_maps[&krate];
        let root_module_id = ModuleId { krate, local_id: def_map.root() };
        let local_id = if let Some((module_index, _)) =
            def_map.modules().iter().find(|(_, module_data)| module_data.location.file == file)
        {
            LocalModuleId(module_index)
        } else {
            def_map.root()
        };
        let module_id = ModuleId { krate, local_id };
        Self {
            file,
            byte_index,
            byte,
            root_module_id,
            module_id,
            def_maps,
            dependencies,
            interner,
            completion_items: Vec::new(),
            local_variables: HashMap::new(),
            type_parameters: HashSet::new(),
        }
    }

    fn find(&mut self, parsed_module: &ParsedModule) -> Option<CompletionResponse> {
        self.find_in_parsed_module(parsed_module);

        if self.completion_items.is_empty() {
            None
        } else {
            let mut items = std::mem::take(&mut self.completion_items);

            // Show items that start with underscore last in the list
            for item in items.iter_mut() {
                if item.label.starts_with('_') {
                    item.sort_text = Some(underscore_sort_text());
                }
            }

            Some(CompletionResponse::Array(items))
        }
    }

    fn find_in_parsed_module(&mut self, parsed_module: &ParsedModule) {
        for item in &parsed_module.items {
            self.find_in_item(item);
        }
    }

    fn find_in_item(&mut self, item: &Item) {
        if !self.includes_span(item.span) {
            return;
        }

        match &item.kind {
            ItemKind::Import(use_tree) => {
                let mut prefixes = Vec::new();
                self.find_in_use_tree(use_tree, &mut prefixes);
            }
            ItemKind::Submodules(parsed_sub_module) => {
                // Switch `self.module_id` to the submodule
                let previous_module_id = self.module_id;

                let def_map = &self.def_maps[&self.module_id.krate];
                let Some(module_data) = def_map.modules().get(self.module_id.local_id.0) else {
                    return;
                };
                if let Some(child_module) = module_data.children.get(&parsed_sub_module.name) {
                    self.module_id =
                        ModuleId { krate: self.module_id.krate, local_id: *child_module };
                }

                self.find_in_parsed_module(&parsed_sub_module.contents);

                // Restore the old module before continuing
                self.module_id = previous_module_id;
            }
            ItemKind::Function(noir_function) => self.find_in_noir_function(noir_function),
            ItemKind::TraitImpl(noir_trait_impl) => self.find_in_noir_trait_impl(noir_trait_impl),
            ItemKind::Impl(type_impl) => self.find_in_type_impl(type_impl),
            ItemKind::Global(let_statement) => self.find_in_let_statement(let_statement, false),
            ItemKind::TypeAlias(noir_type_alias) => self.find_in_noir_type_alias(noir_type_alias),
            ItemKind::Struct(noir_struct) => self.find_in_noir_struct(noir_struct),
            ItemKind::Trait(noir_trait) => self.find_in_noir_trait(noir_trait),
            ItemKind::ModuleDecl(_) => (),
        }
    }

    fn find_in_noir_function(&mut self, noir_function: &NoirFunction) {
        let old_type_parameters = self.type_parameters.clone();
        self.collect_type_parameters_in_generics(&noir_function.def.generics);

        for param in &noir_function.def.parameters {
            self.find_in_unresolved_type(&param.typ);
        }

        self.find_in_function_return_type(&noir_function.def.return_type);

        self.local_variables.clear();
        for param in &noir_function.def.parameters {
            self.collect_local_variables(&param.pattern);
        }

        self.find_in_block_expression(&noir_function.def.body);

        self.type_parameters = old_type_parameters;
    }

    fn find_in_noir_trait_impl(&mut self, noir_trait_impl: &NoirTraitImpl) {
        self.type_parameters.clear();
        self.collect_type_parameters_in_generics(&noir_trait_impl.impl_generics);

        for item in &noir_trait_impl.items {
            self.find_in_trait_impl_item(item);
        }

        self.type_parameters.clear();
    }

    fn find_in_trait_impl_item(&mut self, item: &TraitImplItem) {
        match item {
            TraitImplItem::Function(noir_function) => self.find_in_noir_function(noir_function),
            TraitImplItem::Constant(_, _, _) => (),
            TraitImplItem::Type { .. } => (),
        }
    }

    fn find_in_type_impl(&mut self, type_impl: &TypeImpl) {
        self.type_parameters.clear();
        self.collect_type_parameters_in_generics(&type_impl.generics);

        for (method, span) in &type_impl.methods {
            self.find_in_noir_function(method);

            // Optimization: stop looking in functions past the completion cursor
            if span.end() as usize > self.byte_index {
                break;
            }
        }

        self.type_parameters.clear();
    }

    fn find_in_noir_type_alias(&mut self, noir_type_alias: &NoirTypeAlias) {
        self.find_in_unresolved_type(&noir_type_alias.typ);
    }

    fn find_in_noir_struct(&mut self, noir_struct: &NoirStruct) {
        self.type_parameters.clear();
        self.collect_type_parameters_in_generics(&noir_struct.generics);

        for (_name, unresolved_type) in &noir_struct.fields {
            self.find_in_unresolved_type(unresolved_type);
        }

        self.type_parameters.clear();
    }

    fn find_in_noir_trait(&mut self, noir_trait: &NoirTrait) {
        for item in &noir_trait.items {
            self.find_in_trait_item(item);
        }
    }

    fn find_in_trait_item(&mut self, trait_item: &TraitItem) {
        match trait_item {
            TraitItem::Function {
                name: _,
                generics,
                parameters,
                return_type,
                where_clause,
                body,
            } => {
                let old_type_parameters = self.type_parameters.clone();
                self.collect_type_parameters_in_generics(generics);

                for (_name, unresolved_type) in parameters {
                    self.find_in_unresolved_type(unresolved_type);
                }

                self.find_in_function_return_type(return_type);

                for unresolved_trait_constraint in where_clause {
                    self.find_in_unresolved_type(&unresolved_trait_constraint.typ);
                }

                if let Some(body) = body {
                    self.local_variables.clear();
                    for (name, _) in parameters {
                        self.local_variables.insert(name.to_string(), name.span());
                    }
                    self.find_in_block_expression(body);
                };

                self.type_parameters = old_type_parameters;
            }
            TraitItem::Constant { name: _, typ, default_value } => {
                self.find_in_unresolved_type(typ);

                if let Some(default_value) = default_value {
                    self.find_in_expression(default_value);
                }
            }
            TraitItem::Type { name: _ } => (),
        }
    }

    fn find_in_block_expression(&mut self, block_expression: &BlockExpression) {
        let old_local_variables = self.local_variables.clone();
        for statement in &block_expression.statements {
            self.find_in_statement(statement);

            // Optimization: stop looking in statements past the completion cursor
            if statement.span.end() as usize > self.byte_index {
                break;
            }
        }
        self.local_variables = old_local_variables;
    }

    fn find_in_statement(&mut self, statement: &Statement) {
        match &statement.kind {
            noirc_frontend::ast::StatementKind::Let(let_statement) => {
                self.find_in_let_statement(let_statement, true);
            }
            noirc_frontend::ast::StatementKind::Constrain(constrain_statement) => {
                self.find_in_constrain_statement(constrain_statement);
            }
            noirc_frontend::ast::StatementKind::Expression(expression) => {
                self.find_in_expression(expression);
            }
            noirc_frontend::ast::StatementKind::Assign(assign_statement) => {
                self.find_in_assign_statement(assign_statement);
            }
            noirc_frontend::ast::StatementKind::For(for_loop_statement) => {
                self.find_in_for_loop_statement(for_loop_statement);
            }
            noirc_frontend::ast::StatementKind::Comptime(statement) => {
                // When entering a comptime block, regular local variables shouldn't be offered anymore
                let old_local_variables = self.local_variables.clone();
                self.local_variables.clear();

                self.find_in_statement(statement);

                self.local_variables = old_local_variables;
            }
            noirc_frontend::ast::StatementKind::Semi(expression) => {
                self.find_in_expression(expression);
            }
            noirc_frontend::ast::StatementKind::Break
            | noirc_frontend::ast::StatementKind::Continue
            | noirc_frontend::ast::StatementKind::Error => (),
        }
    }

    fn find_in_let_statement(
        &mut self,
        let_statement: &LetStatement,
        collect_local_variables: bool,
    ) {
        self.find_in_unresolved_type(&let_statement.r#type);
        self.find_in_expression(&let_statement.expression);

        if collect_local_variables {
            self.collect_local_variables(&let_statement.pattern);
        }
    }

    fn find_in_constrain_statement(&mut self, constrain_statement: &ConstrainStatement) {
        self.find_in_expression(&constrain_statement.0);

        if let Some(exp) = &constrain_statement.1 {
            self.find_in_expression(exp);
        }
    }

    fn find_in_assign_statement(
        &mut self,
        assign_statement: &noirc_frontend::ast::AssignStatement,
    ) {
        self.find_in_lvalue(&assign_statement.lvalue);
        self.find_in_expression(&assign_statement.expression);
    }

    fn find_in_for_loop_statement(&mut self, for_loop_statement: &ForLoopStatement) {
        let old_local_variables = self.local_variables.clone();
        let ident = &for_loop_statement.identifier;
        self.local_variables.insert(ident.to_string(), ident.span());

        self.find_in_for_range(&for_loop_statement.range);
        self.find_in_expression(&for_loop_statement.block);

        self.local_variables = old_local_variables;
    }

    fn find_in_lvalue(&mut self, lvalue: &LValue) {
        match lvalue {
            LValue::Ident(ident) => {
                if self.byte == Some(b'.') && ident.span().end() as usize == self.byte_index - 1 {
                    let location = Location::new(ident.span(), self.file);
                    if let Some(ReferenceId::Local(definition_id)) =
                        self.interner.find_referenced(location)
                    {
                        let typ = self.interner.definition_type(definition_id);
                        let prefix = "";
                        self.complete_type_fields_and_methods(&typ, prefix);
                    }
                }
            }
            LValue::MemberAccess { object, field_name: _, span: _ } => self.find_in_lvalue(object),
            LValue::Index { array, index, span: _ } => {
                self.find_in_lvalue(array);
                self.find_in_expression(index);
            }
            LValue::Dereference(lvalue, _) => self.find_in_lvalue(lvalue),
        }
    }

    fn find_in_for_range(&mut self, for_range: &ForRange) {
        match for_range {
            ForRange::Range(start, end) => {
                self.find_in_expression(start);
                self.find_in_expression(end);
            }
            ForRange::Array(expression) => self.find_in_expression(expression),
        }
    }

    fn find_in_expressions(&mut self, expressions: &[Expression]) {
        for expression in expressions {
            self.find_in_expression(expression);
        }
    }

    fn find_in_expression(&mut self, expression: &Expression) {
        match &expression.kind {
            noirc_frontend::ast::ExpressionKind::Literal(literal) => self.find_in_literal(literal),
            noirc_frontend::ast::ExpressionKind::Block(block_expression) => {
                self.find_in_block_expression(block_expression);
            }
            noirc_frontend::ast::ExpressionKind::Prefix(prefix_expression) => {
                self.find_in_expression(&prefix_expression.rhs);
            }
            noirc_frontend::ast::ExpressionKind::Index(index_expression) => {
                self.find_in_index_expression(index_expression);
            }
            noirc_frontend::ast::ExpressionKind::Call(call_expression) => {
                self.find_in_call_expression(call_expression);
            }
            noirc_frontend::ast::ExpressionKind::MethodCall(method_call_expression) => {
                self.find_in_method_call_expression(method_call_expression);
            }
            noirc_frontend::ast::ExpressionKind::Constructor(constructor_expression) => {
                self.find_in_constructor_expression(constructor_expression);
            }
            noirc_frontend::ast::ExpressionKind::MemberAccess(member_access_expression) => {
                self.find_in_member_access_expression(member_access_expression);
            }
            noirc_frontend::ast::ExpressionKind::Cast(cast_expression) => {
                self.find_in_cast_expression(cast_expression);
            }
            noirc_frontend::ast::ExpressionKind::Infix(infix_expression) => {
                self.find_in_infix_expression(infix_expression);
            }
            noirc_frontend::ast::ExpressionKind::If(if_expression) => {
                self.find_in_if_expression(if_expression);
            }
            noirc_frontend::ast::ExpressionKind::Variable(path) => {
                self.find_in_path(path, RequestedItems::AnyItems);
            }
            noirc_frontend::ast::ExpressionKind::Tuple(expressions) => {
                self.find_in_expressions(expressions);
            }
            noirc_frontend::ast::ExpressionKind::Lambda(lambda) => self.find_in_lambda(lambda),
            noirc_frontend::ast::ExpressionKind::Parenthesized(expression) => {
                self.find_in_expression(expression);
            }
            noirc_frontend::ast::ExpressionKind::Unquote(expression) => {
                self.find_in_expression(expression);
            }
            noirc_frontend::ast::ExpressionKind::Comptime(block_expression, _) => {
                // When entering a comptime block, regular local variables shouldn't be offered anymore
                let old_local_variables = self.local_variables.clone();
                self.local_variables.clear();

                self.find_in_block_expression(block_expression);

                self.local_variables = old_local_variables;
            }
            noirc_frontend::ast::ExpressionKind::Unsafe(block_expression, _) => {
                self.find_in_block_expression(block_expression);
            }
            noirc_frontend::ast::ExpressionKind::AsTraitPath(as_trait_path) => {
                self.find_in_as_trait_path(as_trait_path);
            }
            noirc_frontend::ast::ExpressionKind::Quote(_)
            | noirc_frontend::ast::ExpressionKind::Resolved(_)
            | noirc_frontend::ast::ExpressionKind::Error => (),
        }

        // "foo." (no identifier afterwards) is parsed as the expression on the left hand-side of the dot.
        // Here we check if there's a dot at the completion position, and if the expression
        // ends right before the dot. If so, it means we want to complete the expression's type fields and methods.
        // We only do this after visiting nested expressions, because in an expression like `foo & bar.` we want
        // to complete for `bar`, not for `foo & bar`.
        if self.completion_items.is_empty()
            && self.byte == Some(b'.')
            && expression.span.end() as usize == self.byte_index - 1
        {
            let location = Location::new(expression.span, self.file);
            if let Some(typ) = self.interner.type_at_location(location) {
                let typ = typ.follow_bindings();
                let prefix = "";
                self.complete_type_fields_and_methods(&typ, prefix);
            }
        }
    }

    fn find_in_literal(&mut self, literal: &Literal) {
        match literal {
            Literal::Array(array_literal) => self.find_in_array_literal(array_literal),
            Literal::Slice(array_literal) => self.find_in_array_literal(array_literal),
            Literal::Bool(_)
            | Literal::Integer(_, _)
            | Literal::Str(_)
            | Literal::RawStr(_, _)
            | Literal::FmtStr(_)
            | Literal::Unit => (),
        }
    }

    fn find_in_array_literal(&mut self, array_literal: &ArrayLiteral) {
        match array_literal {
            ArrayLiteral::Standard(expressions) => self.find_in_expressions(expressions),
            ArrayLiteral::Repeated { repeated_element, length } => {
                self.find_in_expression(repeated_element);
                self.find_in_expression(length);
            }
        }
    }

    fn find_in_index_expression(&mut self, index_expression: &IndexExpression) {
        self.find_in_expression(&index_expression.collection);
        self.find_in_expression(&index_expression.index);
    }

    fn find_in_call_expression(&mut self, call_expression: &CallExpression) {
        self.find_in_expression(&call_expression.func);
        self.find_in_expressions(&call_expression.arguments);
    }

    fn find_in_method_call_expression(&mut self, method_call_expression: &MethodCallExpression) {
        self.find_in_expression(&method_call_expression.object);
        self.find_in_expressions(&method_call_expression.arguments);
    }

    fn find_in_constructor_expression(&mut self, constructor_expression: &ConstructorExpression) {
        self.find_in_path(&constructor_expression.type_name, RequestedItems::OnlyTypes);

        for (_field_name, expression) in &constructor_expression.fields {
            self.find_in_expression(expression);
        }
    }

    fn find_in_member_access_expression(
        &mut self,
        member_access_expression: &MemberAccessExpression,
    ) {
        let ident = &member_access_expression.rhs;

        if self.byte_index == ident.span().end() as usize {
            // Assuming member_access_expression is of the form `foo.bar`, we are right after `bar`
            let location = Location::new(member_access_expression.lhs.span, self.file);
            if let Some(typ) = self.interner.type_at_location(location) {
                let typ = typ.follow_bindings();
                let prefix = ident.to_string();
                self.complete_type_fields_and_methods(&typ, &prefix);
                return;
            }
        }

        self.find_in_expression(&member_access_expression.lhs);
    }

    fn find_in_cast_expression(&mut self, cast_expression: &CastExpression) {
        self.find_in_expression(&cast_expression.lhs);
    }

    fn find_in_infix_expression(&mut self, infix_expression: &InfixExpression) {
        self.find_in_expression(&infix_expression.lhs);
        self.find_in_expression(&infix_expression.rhs);
    }

    fn find_in_if_expression(&mut self, if_expression: &IfExpression) {
        self.find_in_expression(&if_expression.condition);

        let old_local_variables = self.local_variables.clone();
        self.find_in_expression(&if_expression.consequence);
        self.local_variables = old_local_variables;

        if let Some(alternative) = &if_expression.alternative {
            let old_local_variables = self.local_variables.clone();
            self.find_in_expression(alternative);
            self.local_variables = old_local_variables;
        }
    }

    fn find_in_lambda(&mut self, lambda: &Lambda) {
        for (_, unresolved_type) in &lambda.parameters {
            self.find_in_unresolved_type(unresolved_type);
        }

        let old_local_variables = self.local_variables.clone();
        for (pattern, _) in &lambda.parameters {
            self.collect_local_variables(pattern);
        }

        self.find_in_expression(&lambda.body);

        self.local_variables = old_local_variables;
    }

    fn find_in_as_trait_path(&mut self, as_trait_path: &AsTraitPath) {
        self.find_in_path(&as_trait_path.trait_path, RequestedItems::OnlyTypes);
    }

    fn find_in_function_return_type(&mut self, return_type: &FunctionReturnType) {
        match return_type {
            noirc_frontend::ast::FunctionReturnType::Default(_) => (),
            noirc_frontend::ast::FunctionReturnType::Ty(unresolved_type) => {
                self.find_in_unresolved_type(unresolved_type);
            }
        }
    }

    fn find_in_unresolved_types(&mut self, unresolved_type: &[UnresolvedType]) {
        for unresolved_type in unresolved_type {
            self.find_in_unresolved_type(unresolved_type);
        }
    }

    fn find_in_unresolved_type(&mut self, unresolved_type: &UnresolvedType) {
        if let Some(span) = unresolved_type.span {
            if !self.includes_span(span) {
                return;
            }
        }

        match &unresolved_type.typ {
            noirc_frontend::ast::UnresolvedTypeData::Array(_, unresolved_type) => {
                self.find_in_unresolved_type(unresolved_type);
            }
            noirc_frontend::ast::UnresolvedTypeData::Slice(unresolved_type) => {
                self.find_in_unresolved_type(unresolved_type);
            }
            noirc_frontend::ast::UnresolvedTypeData::Parenthesized(unresolved_type) => {
                self.find_in_unresolved_type(unresolved_type);
            }
            noirc_frontend::ast::UnresolvedTypeData::Named(path, unresolved_types, _) => {
                self.find_in_path(path, RequestedItems::OnlyTypes);
                self.find_in_unresolved_types(unresolved_types);
            }
            noirc_frontend::ast::UnresolvedTypeData::TraitAsType(path, unresolved_types) => {
                self.find_in_path(path, RequestedItems::OnlyTypes);
                self.find_in_unresolved_types(unresolved_types);
            }
            noirc_frontend::ast::UnresolvedTypeData::MutableReference(unresolved_type) => {
                self.find_in_unresolved_type(unresolved_type);
            }
            noirc_frontend::ast::UnresolvedTypeData::Tuple(unresolved_types) => {
                self.find_in_unresolved_types(unresolved_types);
            }
            noirc_frontend::ast::UnresolvedTypeData::Function(args, ret, env, _) => {
                self.find_in_unresolved_types(args);
                self.find_in_unresolved_type(ret);
                self.find_in_unresolved_type(env);
            }
            noirc_frontend::ast::UnresolvedTypeData::AsTraitPath(as_trait_path) => {
                self.find_in_as_trait_path(as_trait_path);
            }
            noirc_frontend::ast::UnresolvedTypeData::Expression(_)
            | noirc_frontend::ast::UnresolvedTypeData::FormatString(_, _)
            | noirc_frontend::ast::UnresolvedTypeData::String(_)
            | noirc_frontend::ast::UnresolvedTypeData::Unspecified
            | noirc_frontend::ast::UnresolvedTypeData::Quoted(_)
            | noirc_frontend::ast::UnresolvedTypeData::FieldElement
            | noirc_frontend::ast::UnresolvedTypeData::Integer(_, _)
            | noirc_frontend::ast::UnresolvedTypeData::Bool
            | noirc_frontend::ast::UnresolvedTypeData::Unit
            | noirc_frontend::ast::UnresolvedTypeData::Resolved(_)
            | noirc_frontend::ast::UnresolvedTypeData::Error => (),
        }
    }

    fn find_in_path(&mut self, path: &Path, requested_items: RequestedItems) {
        // Only offer completions if we are right at the end of the path
        if self.byte_index != path.span.end() as usize {
            return;
        }

        let after_colons = self.byte == Some(b':');

        let mut idents: Vec<Ident> =
            path.segments.iter().map(|segment| segment.ident.clone()).collect();
        let prefix;
        let at_root;

        if after_colons {
            prefix = String::new();
            at_root = false;
        } else {
            prefix = idents.pop().unwrap().to_string();
            at_root = idents.is_empty();
        }

        let is_single_segment = !after_colons && idents.is_empty() && path.kind == PathKind::Plain;
        let module_id;

        if idents.is_empty() {
            module_id = self.module_id;
        } else {
            let Some(module_def_id) = self.resolve_path(idents) else {
                return;
            };

            match module_def_id {
                ModuleDefId::ModuleId(id) => module_id = id,
                ModuleDefId::TypeId(struct_id) => {
                    let struct_type = self.interner.get_struct(struct_id);
                    self.complete_type_methods(
                        &Type::Struct(struct_type, vec![]),
                        &prefix,
                        FunctionKind::Any,
                    );
                    return;
                }
                ModuleDefId::FunctionId(_) => {
                    // There's nothing inside a function
                    return;
                }
                ModuleDefId::TypeAliasId(type_alias_id) => {
                    let type_alias = self.interner.get_type_alias(type_alias_id);
                    let type_alias = type_alias.borrow();
                    self.complete_type_methods(&type_alias.typ, &prefix, FunctionKind::Any);
                    return;
                }
                ModuleDefId::TraitId(_) => {
                    // For now we don't suggest trait methods
                    return;
                }
                ModuleDefId::GlobalId(_) => return,
            }
        }

        let module_completion_kind = if after_colons {
            ModuleCompletionKind::DirectChildren
        } else {
            ModuleCompletionKind::AllVisibleItems
        };
        let function_completion_kind = FunctionCompletionKind::NameAndParameters;

        self.complete_in_module(
            module_id,
            &prefix,
            path.kind,
            at_root,
            module_completion_kind,
            function_completion_kind,
            requested_items,
        );

        if is_single_segment {
            match requested_items {
                RequestedItems::AnyItems => {
                    self.local_variables_completion(&prefix);
                    self.builtin_functions_completion(&prefix);
                    self.builtin_values_completion(&prefix);
                }
                RequestedItems::OnlyTypes => {
                    self.builtin_types_completion(&prefix);
                    self.type_parameters_completion(&prefix);
                }
            }
        }
    }

    fn local_variables_completion(&mut self, prefix: &str) {
        for (name, span) in &self.local_variables {
            if name_matches(name, prefix) {
                let location = Location::new(*span, self.file);
                let description = if let Some(ReferenceId::Local(definition_id)) =
                    self.interner.reference_at_location(location)
                {
                    let typ = self.interner.definition_type(definition_id);
                    Some(typ.to_string())
                } else {
                    None
                };

                self.completion_items.push(simple_completion_item(
                    name,
                    CompletionItemKind::VARIABLE,
                    description,
                ));
            }
        }
    }

    fn type_parameters_completion(&mut self, prefix: &str) {
        for name in &self.type_parameters {
            if name_matches(name, prefix) {
                self.completion_items.push(simple_completion_item(
                    name,
                    CompletionItemKind::TYPE_PARAMETER,
                    None,
                ));
            }
        }
    }

    fn find_in_use_tree(&mut self, use_tree: &UseTree, prefixes: &mut Vec<Path>) {
        match &use_tree.kind {
            UseTreeKind::Path(ident, alias) => {
                prefixes.push(use_tree.prefix.clone());
                self.find_in_use_tree_path(prefixes, ident, alias);
                prefixes.pop();
            }
            UseTreeKind::List(use_trees) => {
                prefixes.push(use_tree.prefix.clone());
                for use_tree in use_trees {
                    self.find_in_use_tree(use_tree, prefixes);
                }
                prefixes.pop();
            }
        }
    }

    fn find_in_use_tree_path(
        &mut self,
        prefixes: &Vec<Path>,
        ident: &Ident,
        alias: &Option<Ident>,
    ) {
        if let Some(_alias) = alias {
            // Won't handle completion if there's an alias (for now)
            return;
        }

        let after_colons = self.byte == Some(b':');
        let at_ident_end = self.byte_index == ident.span().end() as usize;
        let at_ident_colons_end =
            after_colons && self.byte_index - 2 == ident.span().end() as usize;

        if !(at_ident_end || at_ident_colons_end) {
            return;
        }

        let path_kind = prefixes[0].kind;

        let mut segments: Vec<Ident> = Vec::new();
        for prefix in prefixes {
            for segment in &prefix.segments {
                segments.push(segment.ident.clone());
            }
        }

        let module_completion_kind = ModuleCompletionKind::DirectChildren;
        let function_completion_kind = FunctionCompletionKind::Name;
        let requested_items = RequestedItems::AnyItems;

        if after_colons {
            // We are right after "::"
            segments.push(ident.clone());

            if let Some(module_id) = self.resolve_module(segments) {
                let prefix = String::new();
                let at_root = false;
                self.complete_in_module(
                    module_id,
                    &prefix,
                    path_kind,
                    at_root,
                    module_completion_kind,
                    function_completion_kind,
                    requested_items,
                );
            };
        } else {
            // We are right after the last segment
            let prefix = ident.to_string();
            if segments.is_empty() {
                let at_root = true;
                self.complete_in_module(
                    self.module_id,
                    &prefix,
                    path_kind,
                    at_root,
                    module_completion_kind,
                    function_completion_kind,
                    requested_items,
                );
            } else if let Some(module_id) = self.resolve_module(segments) {
                let at_root = false;
                self.complete_in_module(
                    module_id,
                    &prefix,
                    path_kind,
                    at_root,
                    module_completion_kind,
                    function_completion_kind,
                    requested_items,
                );
            }
        }
    }

    fn collect_local_variables(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Identifier(ident) => {
                self.local_variables.insert(ident.to_string(), ident.span());
            }
            Pattern::Mutable(pattern, _, _) => self.collect_local_variables(pattern),
            Pattern::Tuple(patterns, _) => {
                for pattern in patterns {
                    self.collect_local_variables(pattern);
                }
            }
            Pattern::Struct(_, patterns, _) => {
                for (_, pattern) in patterns {
                    self.collect_local_variables(pattern);
                }
            }
        }
    }

    fn collect_type_parameters_in_generics(&mut self, generics: &UnresolvedGenerics) {
        for generic in generics {
            self.collect_type_parameters_in_generic(generic);
        }
    }

    fn collect_type_parameters_in_generic(&mut self, generic: &UnresolvedGeneric) {
        match generic {
            UnresolvedGeneric::Variable(ident) => {
                self.type_parameters.insert(ident.to_string());
            }
            UnresolvedGeneric::Numeric { ident, typ: _ } => {
                self.type_parameters.insert(ident.to_string());
            }
            UnresolvedGeneric::Resolved(..) => (),
        };
    }

    fn complete_type_fields_and_methods(&mut self, typ: &Type, prefix: &str) {
        match typ {
            Type::Struct(struct_type, generics) => {
                self.complete_struct_fields(&struct_type.borrow(), generics, prefix);
            }
            Type::MutableReference(typ) => {
                return self.complete_type_fields_and_methods(typ, prefix);
            }
            Type::Alias(type_alias, _) => {
                let type_alias = type_alias.borrow();
                return self.complete_type_fields_and_methods(&type_alias.typ, prefix);
            }
            Type::FieldElement
            | Type::Array(_, _)
            | Type::Slice(_)
            | Type::Integer(_, _)
            | Type::Bool
            | Type::String(_)
            | Type::FmtString(_, _)
            | Type::Unit
            | Type::Tuple(_)
            | Type::TypeVariable(_, _)
            | Type::TraitAsType(_, _, _)
            | Type::NamedGeneric(_, _, _)
            | Type::Function(..)
            | Type::Forall(_, _)
            | Type::Constant(_)
            | Type::Quoted(_)
            | Type::InfixExpr(_, _, _)
            | Type::Error => (),
        }

        self.complete_type_methods(typ, prefix, FunctionKind::SelfType(typ));
    }

    fn complete_type_methods(&mut self, typ: &Type, prefix: &str, function_kind: FunctionKind) {
        let Some(methods_by_name) = self.interner.get_type_methods(typ) else {
            return;
        };

        for (name, methods) in methods_by_name {
            for func_id in methods.iter() {
                if name_matches(name, prefix) {
                    if let Some(completion_item) = self.function_completion_item(
                        func_id,
                        FunctionCompletionKind::NameAndParameters,
                        function_kind,
                    ) {
                        self.completion_items.push(completion_item);
                    }
                }
            }
        }
    }

    fn complete_struct_fields(
        &mut self,
        struct_type: &StructType,
        generics: &[Type],
        prefix: &str,
    ) {
        for (name, typ) in struct_type.get_fields(generics) {
            if name_matches(&name, prefix) {
                self.completion_items.push(simple_completion_item(
                    name,
                    CompletionItemKind::FIELD,
                    Some(typ.to_string()),
                ));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn complete_in_module(
        &mut self,
        module_id: ModuleId,
        prefix: &str,
        path_kind: PathKind,
        at_root: bool,
        module_completion_kind: ModuleCompletionKind,
        function_completion_kind: FunctionCompletionKind,
        requested_items: RequestedItems,
    ) {
        let def_map = &self.def_maps[&module_id.krate];
        let Some(mut module_data) = def_map.modules().get(module_id.local_id.0) else {
            return;
        };

        if at_root {
            match path_kind {
                PathKind::Crate => {
                    let Some(root_module_data) = def_map.modules().get(def_map.root().0) else {
                        return;
                    };
                    module_data = root_module_data;
                }
                PathKind::Super => {
                    let Some(parent) = module_data.parent else {
                        return;
                    };
                    let Some(parent_module_data) = def_map.modules().get(parent.0) else {
                        return;
                    };
                    module_data = parent_module_data;
                }
                PathKind::Dep => (),
                PathKind::Plain => (),
            }
        }

        let function_kind = FunctionKind::Any;

        let items = match module_completion_kind {
            ModuleCompletionKind::DirectChildren => module_data.definitions(),
            ModuleCompletionKind::AllVisibleItems => module_data.scope(),
        };

        for ident in items.names() {
            let name = &ident.0.contents;

            if name_matches(name, prefix) {
                let per_ns = module_data.find_name(ident);
                if let Some((module_def_id, _, _)) = per_ns.types {
                    if let Some(completion_item) = self.module_def_id_completion_item(
                        module_def_id,
                        name.clone(),
                        function_completion_kind,
                        function_kind,
                        requested_items,
                    ) {
                        self.completion_items.push(completion_item);
                    }
                }

                if let Some((module_def_id, _, _)) = per_ns.values {
                    if let Some(completion_item) = self.module_def_id_completion_item(
                        module_def_id,
                        name.clone(),
                        function_completion_kind,
                        function_kind,
                        requested_items,
                    ) {
                        self.completion_items.push(completion_item);
                    }
                }
            }
        }

        if at_root && path_kind == PathKind::Plain {
            for dependency in self.dependencies {
                let dependency_name = dependency.as_name();
                if name_matches(&dependency_name, prefix) {
                    self.completion_items.push(crate_completion_item(dependency_name));
                }
            }

            if name_matches("crate::", prefix) {
                self.completion_items.push(simple_completion_item(
                    "crate::",
                    CompletionItemKind::KEYWORD,
                    None,
                ));
            }

            if module_data.parent.is_some() && name_matches("super::", prefix) {
                self.completion_items.push(simple_completion_item(
                    "super::",
                    CompletionItemKind::KEYWORD,
                    None,
                ));
            }
        }
    }

    fn resolve_module(&self, segments: Vec<Ident>) -> Option<ModuleId> {
        if let Some(ModuleDefId::ModuleId(module_id)) = self.resolve_path(segments) {
            Some(module_id)
        } else {
            None
        }
    }

    fn resolve_path(&self, segments: Vec<Ident>) -> Option<ModuleDefId> {
        let last_segment = segments.last().unwrap().clone();

        let path_segments = segments.into_iter().map(PathSegment::from).collect();
        let path = Path { segments: path_segments, kind: PathKind::Plain, span: Span::default() };

        let path_resolver = StandardPathResolver::new(self.root_module_id);
        if let Ok(path_resolution) = path_resolver.resolve(self.def_maps, path, &mut None) {
            return Some(path_resolution.module_def_id);
        }

        // If we can't resolve a path trough lookup, let's see if the last segment is bound to a type
        let location = Location::new(last_segment.span(), self.file);
        if let Some(reference_id) = self.interner.find_referenced(location) {
            if let Some(id) = module_def_id_from_reference_id(reference_id) {
                return Some(id);
            }
        }

        None
    }

    fn includes_span(&self, span: Span) -> bool {
        span.start() as usize <= self.byte_index && self.byte_index <= span.end() as usize
    }
}

fn name_matches(name: &str, prefix: &str) -> bool {
    name.starts_with(prefix)
}

fn module_def_id_from_reference_id(reference_id: ReferenceId) -> Option<ModuleDefId> {
    match reference_id {
        ReferenceId::Module(module_id) => Some(ModuleDefId::ModuleId(module_id)),
        ReferenceId::Struct(struct_id) => Some(ModuleDefId::TypeId(struct_id)),
        ReferenceId::Trait(trait_id) => Some(ModuleDefId::TraitId(trait_id)),
        ReferenceId::Function(func_id) => Some(ModuleDefId::FunctionId(func_id)),
        ReferenceId::Alias(type_alias_id) => Some(ModuleDefId::TypeAliasId(type_alias_id)),
        ReferenceId::StructMember(_, _)
        | ReferenceId::Global(_)
        | ReferenceId::Local(_)
        | ReferenceId::Reference(_, _) => None,
    }
}

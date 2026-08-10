//! Reading structure out of one `__d(...)` module with the oxc AST.
//!
//! Everything here is deliberately structural rather than textual. The bundle is
//! minified, so identifiers are single letters and the same three characters mean
//! different things in different modules: a regex for `e\.(\w+)\s*=` finds export
//! assignments in one module and unrelated local writes in the next. Resolving the
//! factory's *parameters* instead — Metro calls the factory as
//! `factory(global, require, importDefault, importAll, module, exports, dependencyMap)`
//! — makes "assignment onto exports" a question about a binding, and it stays
//! correct whatever the minifier named it.

use oxc_ast::ast::{
    Argument, AssignmentTarget, BindingPattern, Expression, Function, Program, Statement,
};
use oxc_ast_visit::{Visit, walk};

/// The Metro module-definition function.
pub const DEFINE_FN: &str = "__d";

/// Positional meaning of the factory's parameters, per Metro's calling convention.
const MODULE_PARAM: usize = 4;
const EXPORTS_PARAM: usize = 5;

/// What one module declares.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ModuleFacts {
    pub name: String,
    /// Dependency names in declaration order — positional, matching `d[i]`.
    pub deps: Vec<String>,
    /// Names assigned onto the exports object, sorted and deduplicated.
    pub exports: Vec<String>,
    /// Named functions declared at the top level of the factory, sorted.
    pub functions: Vec<String>,
}

/// Pull the facts out of a program that consists of a single `__d(...)` call.
///
/// Returns `None` when the program is not a module definition, which is a real
/// case (bundle bootstrap code) and not an error.
pub fn facts_of_module(program: &Program<'_>) -> Option<ModuleFacts> {
    for stmt in &program.body {
        let Statement::ExpressionStatement(es) = stmt else {
            continue;
        };
        let Expression::CallExpression(call) = &es.expression else {
            continue;
        };
        let Expression::Identifier(callee) = &call.callee else {
            continue;
        };
        if callee.name.as_str() != DEFINE_FN || call.arguments.len() < 3 {
            continue;
        }

        let Some(Expression::StringLiteral(name)) = call.arguments[0].as_expression() else {
            continue;
        };

        let mut facts = ModuleFacts {
            name: name.value.to_string(),
            ..Default::default()
        };

        if let Some(Expression::ArrayExpression(arr)) = call.arguments[1].as_expression() {
            for el in &arr.elements {
                if let Some(Expression::StringLiteral(s)) = el.as_expression() {
                    facts.deps.push(s.value.to_string());
                }
            }
        }

        if let Some(factory) = factory_function(&call.arguments[2]) {
            let mut scan = FactoryScan::new(factory);
            if let Some(body) = &factory.body {
                scan.visit_function_body(body);
                scan.collect_top_level(body);
            }
            facts.exports = sorted_unique(scan.exports);
            facts.functions = sorted_unique(scan.functions);
        }

        return Some(facts);
    }
    None
}

fn sorted_unique(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

fn factory_function<'b, 'a>(arg: &'b Argument<'a>) -> Option<&'b Function<'a>> {
    match arg.as_expression()? {
        Expression::FunctionExpression(f) => Some(f),
        _ => None,
    }
}

/// Name of a simple identifier parameter at `position`, if it is one.
fn param_name<'a>(factory: &Function<'a>, position: usize) -> Option<&'a str> {
    let param = factory.params.items.get(position)?;
    match &param.pattern {
        BindingPattern::BindingIdentifier(id) => Some(id.name.as_str()),
        _ => None,
    }
}

struct FactoryScan<'a> {
    /// The binding the factory receives `exports` as, e.g. `e`.
    exports_binding: Option<&'a str>,
    /// The binding the factory receives `module` as, e.g. `m`.
    module_binding: Option<&'a str>,
    exports: Vec<String>,
    functions: Vec<String>,
}

impl<'a> FactoryScan<'a> {
    fn new(factory: &Function<'a>) -> Self {
        Self {
            exports_binding: param_name(factory, EXPORTS_PARAM),
            module_binding: param_name(factory, MODULE_PARAM),
            exports: Vec::new(),
            functions: Vec::new(),
        }
    }

    /// Top-level declarations of the factory body — the module's own functions,
    /// as opposed to every closure nested anywhere inside it.
    fn collect_top_level(&mut self, body: &oxc_ast::ast::FunctionBody<'a>) {
        for stmt in &body.statements {
            match stmt {
                Statement::FunctionDeclaration(f) => {
                    if let Some(id) = &f.id {
                        self.functions.push(id.name.to_string());
                    }
                }
                Statement::VariableDeclaration(decl) => {
                    for d in &decl.declarations {
                        let BindingPattern::BindingIdentifier(id) = &d.id else {
                            continue;
                        };
                        let is_callable = matches!(
                            d.init,
                            Some(Expression::FunctionExpression(_))
                                | Some(Expression::ArrowFunctionExpression(_))
                                | Some(Expression::ClassExpression(_))
                        );
                        if is_callable {
                            self.functions.push(id.name.to_string());
                        }
                    }
                }
                Statement::ClassDeclaration(c) => {
                    if let Some(id) = &c.id {
                        self.functions.push(id.name.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    /// `<exports>.name = …` → `name`; `<module>.exports = …` → `default`;
    /// `<module>.exports.name = …` → `name`.
    fn record_export_target(&mut self, target: &AssignmentTarget<'a>) {
        let Some(member) = target.as_member_expression() else {
            return;
        };
        let Some(prop) = member.static_property_name() else {
            return;
        };

        // `<exports>.prop = …`
        if let Expression::Identifier(obj) = member.object()
            && Some(obj.name.as_str()) == self.exports_binding
        {
            self.exports.push(prop.to_string());
            return;
        }

        // `<module>.exports = …`
        if let Expression::Identifier(obj) = member.object()
            && Some(obj.name.as_str()) == self.module_binding
            && prop == "exports"
        {
            self.exports.push("default".to_string());
            return;
        }

        // `<module>.exports.prop = …`
        if let Some(inner) = member.object().as_member_expression()
            && inner.static_property_name() == Some("exports")
            && let Expression::Identifier(obj) = inner.object()
            && Some(obj.name.as_str()) == self.module_binding
        {
            self.exports.push(prop.to_string());
        }
    }
}

impl<'a> Visit<'a> for FactoryScan<'a> {
    fn visit_assignment_expression(&mut self, expr: &oxc_ast::ast::AssignmentExpression<'a>) {
        self.record_export_target(&expr.left);
        walk::walk_assignment_expression(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn facts(src: &str) -> ModuleFacts {
        let alloc = Allocator::default();
        let ret = Parser::new(&alloc, src, SourceType::cjs()).parse();
        facts_of_module(&ret.program).expect("a module definition")
    }

    #[test]
    fn reads_name_and_positional_deps() {
        let f = facts(r#"__d("WAWebFoo",["A","B"],function(g,r,i,a,m,e,d){},1);"#);
        assert_eq!(f.name, "WAWebFoo");
        assert_eq!(f.deps, ["A", "B"], "dependency order is positional");
    }

    #[test]
    fn exports_are_resolved_through_the_factory_binding_not_a_letter() {
        // The minifier named exports `q` here; a regex keyed on `e.` finds nothing,
        // and worse, would pick up the unrelated `e.detail` write.
        let f = facts(
            r#"__d("M",[],function(g,r,i,a,mod,q,d){
                 function handler(e){ e.detail = 1; }
                 q.sendStanza = handler;
                 q.parse = function(){};
               },1);"#,
        );
        assert_eq!(f.exports, ["parse", "sendStanza"]);
    }

    #[test]
    fn module_exports_forms_are_recognised() {
        let f = facts(r#"__d("M",[],function(g,r,i,a,m,e,d){ m.exports = {}; },1);"#);
        assert_eq!(f.exports, ["default"]);

        let f = facts(r#"__d("M",[],function(g,r,i,a,m,e,d){ m.exports.parse = 1; },1);"#);
        assert_eq!(f.exports, ["parse"]);
    }

    #[test]
    fn functions_are_top_level_only() {
        let f = facts(
            r#"__d("M",[],function(g,r,i,a,m,e,d){
                 function outer(){ function inner(){} }
                 var arrow = () => {};
                 var notAFunction = 42;
                 class Thing {}
               },1);"#,
        );
        assert_eq!(
            f.functions,
            ["Thing", "arrow", "outer"],
            "nested `inner` is not the module's own surface"
        );
    }

    #[test]
    fn a_non_module_program_is_not_an_error() {
        let alloc = Allocator::default();
        let ret = Parser::new(&alloc, "window.__bootstrap = 1;", SourceType::cjs()).parse();
        assert!(facts_of_module(&ret.program).is_none());
    }

    #[test]
    fn a_factory_with_too_few_params_still_yields_name_and_deps() {
        let f = facts(r#"__d("M",["A"],function(){},1);"#);
        assert_eq!(f.name, "M");
        assert_eq!(f.deps, ["A"]);
        assert!(f.exports.is_empty());
    }
}

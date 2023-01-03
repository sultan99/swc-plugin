use swc_core::ecma::{
    transforms::testing::test,
    visit::{Visit, VisitWith},
};
use std::collections::HashSet;

use swc_core::{
    common::{DUMMY_SP},
    ecma::{
        parser::{Syntax, TsConfig},
        ast::*,
        atoms::JsWord,
        utils::{ExprFactory},
        visit::{Fold, FoldWith},
    },
    plugin::{
        plugin_transform,
        proxies::TransformPluginProgramMetadata,
    },
};
use swc_core::ecma::utils::quote_ident;

mod utils;
mod builder;
mod tokens;
mod ecma_utils;

use builder::*;
use ecma_utils::*;
use crate::tokens::{Icu, IcuChoice, MsgToken, TagOpening};

const LINGUI_T: &str = &"t";


fn dedup_values(mut v: Vec<ValueWithPlaceholder>) -> Vec<ValueWithPlaceholder> {
    let mut uniques = HashSet::new();
    v.retain(|e| uniques.insert(e.placeholder.clone()));

    v
}

fn is_lingui_fn(name: &str) -> bool {
    // todo: i didn't find a better way to create a constant hashmap
    match name {
        "plural" | "select" | "selectOrdinal" => true,
        _ => false,
    }
}


// <Plural /> <Select /> <SelectOrdinal />
fn transform_icu_jsx_macro<'a>(el: &JSXOpeningElement) -> Vec<IcuChoice> {
    let mut choices: Vec<IcuChoice> = Vec::new();

    for attr in &el.attrs {
        if let JSXAttrOrSpread::JSXAttr(attr) = attr {
            if let Some(attr_value) = &attr.value {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    // todo: probably need blacklist more properties, or whitelist only selected
                    if ident.sym.to_string() == "value" {
                        continue;
                    }

                    let mut tokens: Vec<MsgToken> = Vec::new();

                    match attr_value {
                        // some="# books"
                        JSXAttrValue::Lit(Lit::Str(str)) => {
                            let string: String = str.value.clone().to_string();
                            tokens.push(MsgToken::String(string));
                        }

                        JSXAttrValue::JSXExprContainer(JSXExprContainer { expr: JSXExpr::Expr(exp), .. }) => {
                            match exp.as_ref() {
                                // some={"# books"}
                                Expr::Lit(Lit::Str(str)) => {
                                    tokens.push(MsgToken::String(str.value.clone().to_string()))
                                    // choice.builder.push_msg(&str.value);
                                }
                                // some={`# books ${name}`}
                                // Expr::Tpl(tpl) => {
                                //     let (msg, values) = self.transform_tpl_to_msg_and_values(tpl);
                                //     all_values.extend(values);
                                //     push_part(&msg);
                                // }
                                // some={`<Books />`}
                                Expr::JSXElement(_lit) => {}

                                _ => {
                                    // todo: unsupported syntax
                                }
                            }
                        }

                        _ => {
                            // todo unsupported syntax
                        }
                    }

                    choices.push(IcuChoice {
                        tokens,
                        key: ident.sym.clone().to_string(),
                    })
                }
            }
        } else {
            // todo here is spread which is not supported
        }
    }

    return choices;
    // let msg = format!("{{{}, {}, {}}}", "count", "plural", icu_parts.join(" "));
}


#[derive(Default)]
pub struct TransformVisitor {
    has_lingui_macro_imports: bool,
    should_add_18n_import: bool,
    should_add_trans_import: bool,
}

impl TransformVisitor {
    // Receive an expression which expected to be either simple variable (ident) or expression
    // If simple variable is detected os literal used as placeholder
    // If expression detected we use index as placeholder.
    fn get_value_with_placeholder(&self, expr: Box<Expr>, i: &usize) -> ValueWithPlaceholder {
        match expr.as_ref() {
            // `text {foo} bar`
            Expr::Ident(ident) => {
                ValueWithPlaceholder {
                    placeholder: ident.sym.to_string(),
                    value: expr,
                }
            }
            // everything else, e.q.
            // `text {executeFn()} bar`
            // `text {bar.baz} bar`
            _ => {
                // would be a positional argument
                let index_str = &i.to_string();
                ValueWithPlaceholder {
                    placeholder: index_str.into(),
                    value: expr,
                }
            }
        }
    }

    // Receive TemplateLiteral with variables and return plane string where
    // substitutions replaced to placeholders and variables extracted as separate Vec
    // `Hello ${username}!` ->  (msg: `Hello {username}!`, variables: {username})
    fn transform_tpl_to_msg_and_values(&self, tpl: &Tpl) -> (String, Vec<ValueWithPlaceholder>) {
        let mut message = String::new();
        let mut values: Vec<ValueWithPlaceholder> = Vec::with_capacity(tpl.exprs.len());

        for (i, tpl_element) in tpl.quasis.iter().enumerate() {
            message.push_str(&tpl_element.raw);

            if let Some(exp) = tpl.exprs.get(i) {
                let val = self.get_value_with_placeholder(exp.clone(), &i);
                message.push_str(&format!("{{{}}}", &val.placeholder));
                values.push(val);
            }
        }

        (message, values)
    }

    fn create_i18n_fn_call(&self, callee_obj: Box<Expr>, message: &str, values: Vec<ValueWithPlaceholder>) -> CallExpr {
        return CallExpr {
            span: DUMMY_SP,
            callee: Expr::Member(MemberExpr {
                span: DUMMY_SP,
                obj: callee_obj,
                prop: MemberProp::Ident(Ident::new("_".into(), DUMMY_SP)),
            }).as_callee(),
            args: vec![
                message.as_arg(),
                Expr::Object(ObjectLit {
                    span: DUMMY_SP,
                    props: dedup_values(values).into_iter().map(|v| v.to_prop()).collect(),
                }).as_arg(),
            ],
            type_args: None,
        };
    }

    // receive ObjectLiteral {few: "..", many: "..", other: ".."} and create ICU string in form:
    // {count, plural, few {..} many {..} other {..}}
    // If messages passed as TemplateLiterals with variables, it extracts variables into Vec
    // (msg: {count, plural, one `{name} has # friend` other `{name} has # friends`}, variables: {name})
    fn get_icu_from_choices_obj(&self, props: &Vec<PropOrSpread>, icu_value_ident: &JsWord, icu_method: &JsWord) -> (String, Vec<ValueWithPlaceholder>) {
        let mut icu_parts: Vec<String> = Vec::with_capacity(props.len());
        let mut all_values: Vec<ValueWithPlaceholder> = Vec::new();

        for prop_or_spread in props {
            if let PropOrSpread::Prop(prop) = prop_or_spread {
                if let Prop::KeyValue(prop) = prop.as_ref() {
                    if let PropName::Ident(ident) = &prop.key {
                        let mut push_part = |msg: &str| {
                            icu_parts.push(format!("{} {{{}}}", &ident.sym, msg));
                        };

                        // String Literal: "has # friend"
                        if let Expr::Lit(lit) = prop.value.as_ref() {
                            if let Lit::Str(str) = lit {
                                // one {has # friend}
                                push_part(&str.value);
                            }
                        }

                        // Template Literal: `${name} has # friend`
                        if let Expr::Tpl(tpl) = prop.value.as_ref() {
                            let (msg, values) = self.transform_tpl_to_msg_and_values(tpl);
                            all_values.extend(values);
                            push_part(&msg);
                        }
                    } else {
                        // todo panic
                    }
                    // icuParts.push_str(prop.key)
                } else {
                    // todo: panic here we could not parse anything else then KeyValue pair
                }
            } else {
                // todo: panic here, we could not parse spread
            }
        }

        let msg = format!("{{{}, {}, {}}}", icu_value_ident, icu_method, icu_parts.join(" "));

        (msg, all_values)
    }

    // <Trans>Message</Trans>
    fn transform_trans_jsx_macro(&mut self, el: JSXElement) -> JSXElement {
        let mut trans_visitor = TransJSXVisitor::new();

        el.children.visit_children_with(&mut trans_visitor);

        let mut builder = MessageBuilder::new(trans_visitor.tokens);

        // let mut message_builder = trans_visitor.builder;
        // println!("{}", utils::normalize_whitespaces(&message_builder.message));

        let mut id: Option<&JsWord> = None;

        for el in &el.opening.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = el {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    if &ident.sym == "id" {
                        id = Some(&ident.sym)
                    }
                }
            } else {
                // todo panic unsupported syntax JSXAttrSpread
            }
        }

        let mut attrs = vec![
            create_jsx_attribute(
                if let Some(_) = id { "message" } else { "id" }.into(),
                Expr::Lit(Lit::Str(Str {
                    span: DUMMY_SP,
                    value: utils::normalize_whitespaces(&builder.message).into(),
                    // value: builder.message.into(),
                    raw: None,
                })),
            ),
        ];

        builder.values.append(&mut builder.values_indexed);

        if builder.values.len() > 0 {
            attrs.push(create_jsx_attribute(
                "values",
                Expr::Object(ObjectLit {
                    span: DUMMY_SP,
                    props: dedup_values(builder.values).into_iter().map(|item| item.to_prop()).collect(),
                }),
            ))
        }

        if builder.components.len() > 0 {
            attrs.push(create_jsx_attribute(
                "components",
                Expr::Object(ObjectLit {
                    span: DUMMY_SP,
                    props: builder.components.into_iter().map(|item| item.to_prop()).collect(),
                }),
            ))
        }

        attrs.extend(el.opening.attrs);

        self.should_add_trans_import = true;

        return JSXElement {
            span: el.span,
            children: vec![],
            closing: None,
            opening: JSXOpeningElement {
                self_closing: true,
                span: el.opening.span,
                name: el.opening.name,
                type_args: None,
                attrs,
            },
        };
    }
}


struct TransJSXVisitor {
    tokens: Vec<MsgToken>,
}

impl TransJSXVisitor {
    fn new() -> TransJSXVisitor {
        TransJSXVisitor {
            tokens: Vec::new(),
        }
    }
}


impl Visit for TransJSXVisitor {
    // todo: how to handle fragments?
    fn visit_jsx_opening_element(&mut self, el: &JSXOpeningElement) {
        if match_jsx_name(el, "Plural") {
            let value = match get_jsx_attr_value(&el, "value") {
                Some(
                    JSXAttrValue::JSXExprContainer(
                        JSXExprContainer { expr: JSXExpr::Expr(exp), .. }
                    )
                ) => {
                    exp.clone()
                }
                // todo: support here <Plural value=5 >
                // JSXAttrValue::Lit(lit) => {
                //     Box::new(Expr::Lit(*lit))
                // }
                _ => {
                    Box::new(Expr::Lit(Lit::Null(Null {
                        span: DUMMY_SP
                    })))
                }
            };

            let choices = transform_icu_jsx_macro(el);

            self.tokens.push(MsgToken::Icu(Icu {
                choices,
                // todo: support different ICU methods
                icu_method: "plural".into(),
                value,
            }))
        } else {
            self.tokens.push(MsgToken::TagOpening(TagOpening {
                self_closing: el.self_closing,
                el: JSXOpeningElement {
                    self_closing: true,
                    name: el.name.clone(),
                    attrs: el.attrs.clone(),
                    span: el.span,
                    type_args: el.type_args.clone(),
                },
            }));
        }
    }

    fn visit_jsx_closing_element(&mut self, _el: &JSXClosingElement) {
        self.tokens.push(
            MsgToken::TagClosing
        );
    }

    fn visit_jsx_text(&mut self, el: &JSXText) {
        self.tokens.push(
            MsgToken::String(el.value.to_string())
        );
    }

    fn visit_jsx_expr_container(&mut self, cont: &JSXExprContainer) {
        if let JSXExpr::Expr(exp) = &cont.expr {
            match exp.as_ref() {
                Expr::Lit(Lit::Str(str)) => {
                    self.tokens.push(
                        MsgToken::String(str.value.to_string())
                    );
                }
                _ => {
                    self.tokens.push(
                        MsgToken::Value(exp.clone())
                    );
                }
            }
        }
    }
}


impl Fold for TransformVisitor {
    fn fold_module_items(&mut self, mut n: Vec<ModuleItem>) -> Vec<ModuleItem> {
        let mut has_i18n_import = false;
        let mut has_trans_import = false;

        n.retain(|m| {
            if let ModuleItem::ModuleDecl(ModuleDecl::Import(imp)) = m {
                // drop macro imports
                if &imp.src.value == "@lingui/macro" {
                    self.has_lingui_macro_imports = true;
                    return false;
                }

                if &imp.src.value == "@lingui/core" && !imp.type_only {
                    for spec in &imp.specifiers {
                        if let ImportSpecifier::Named(spec) = spec {
                            has_i18n_import = if !has_i18n_import { &spec.local.sym == "i18n" } else { true };
                            has_trans_import = if !has_trans_import { &spec.local.sym == "Trans" } else { true };
                        }
                    }
                }
            }

            true
        });

        println!("{} {}", has_i18n_import, has_trans_import);

        n = n.fold_children_with(self);

        let mut specifiers: Vec<ImportSpecifier> = Vec::new();

        if !has_i18n_import && self.should_add_18n_import {
            specifiers.push(
                ImportSpecifier::Named(ImportNamedSpecifier {
                    span: DUMMY_SP,
                    local: quote_ident!("i18n"),
                    imported: None,
                    is_type_only: false,
                })
            )
        }

        if !has_trans_import && self.should_add_trans_import {
            specifiers.push(
                ImportSpecifier::Named(ImportNamedSpecifier {
                    span: DUMMY_SP,
                    local: quote_ident!("Trans"),
                    imported: None,
                    is_type_only: false,
                })
            )
        }

        if specifiers.len() > 0 {
            n.insert(0, ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
                span: DUMMY_SP,
                specifiers,
                src: Box::new(Str {
                    span: DUMMY_SP,
                    value: "@lingui/core".into(),
                    raw: None,
                }),
                asserts: None,
                type_only: false,
            })));
        }

        n
    }

    fn fold_expr(&mut self, expr: Expr) -> Expr {
        // If no package that we care about is imported, skip the following
        // transformation logic.
        if !self.has_lingui_macro_imports {
            return expr;
        }

        if let Expr::TaggedTpl(tagged_tpl) = &expr {
            match tagged_tpl.tag.as_ref() {
                // t(i18n)``
                Expr::Call(call) if match_callee_name(call, LINGUI_T) => {
                    if let Some(v) = call.args.get(0) {
                        let (message, values)
                            = self.transform_tpl_to_msg_and_values(&tagged_tpl.tpl);
                        return Expr::Call(self.create_i18n_fn_call(
                            v.expr.clone(),
                            &message,
                            values,
                        ));
                    }
                }
                // t``
                Expr::Ident(ident) if &ident.sym == LINGUI_T => {
                    let (message, values)
                        = self.transform_tpl_to_msg_and_values(&tagged_tpl.tpl);

                    self.should_add_18n_import = true;
                    return Expr::Call(self.create_i18n_fn_call(
                        Box::new(Ident::new("i18n".into(), DUMMY_SP).into()),
                        &message,
                        values,
                    ));
                }
                _ => {}
            }
        }

        expr.fold_children_with(self)
    }

    fn fold_call_expr(&mut self, expr: CallExpr) -> CallExpr {
        // If no package that we care about is imported, skip the following
        // transformation logic.
        if !self.has_lingui_macro_imports {
            return expr;
        }

        if let Callee::Expr(e) = &expr.callee {
            match e.as_ref() {
                // (plural | select | selectOrdinal)()
                Expr::Ident(ident) => {
                    if !is_lingui_fn(&ident.sym) {
                        return expr;
                    }

                    if expr.args.len() != 2 {
                        // malformed plural call, exit
                        return expr;
                    }

                    // ICU value
                    let arg = expr.args.get(0).unwrap();
                    let icu_value
                        = self.get_value_with_placeholder(arg.expr.clone(), &0);

                    // ICU Choices
                    let arg = expr.args.get(1).unwrap();
                    if let Expr::Object(object) = &arg.expr.as_ref() {
                        let (message, values) = self.get_icu_from_choices_obj(
                            &object.props, &icu_value.placeholder.clone().into(), &ident.sym);

                        // todo need a function to remove duplicates from arguments
                        let mut all_values = vec![icu_value];
                        all_values.extend(values);

                        self.should_add_18n_import = true;

                        return self.create_i18n_fn_call(
                            Box::new(Ident::new("i18n".into(), DUMMY_SP).into()),
                            &message,
                            all_values,
                        );
                    } else {
                        // todo passed not an ObjectLiteral,
                        //      we should panic here or just skip this call
                    }
                }
                _ => {}
            }
        }

        expr
    }

    fn fold_jsx_element(&mut self, el: JSXElement) -> JSXElement {
        // If no package that we care about is imported, skip the following
        // transformation logic.
        if !self.has_lingui_macro_imports {
            return el;
        }

        if let JSXElementName::Ident(ident) = &el.opening.name {
            if &ident.sym == "Trans" {
                return self.transform_trans_jsx_macro(el);
            }

            // if (&ident.sym == "Plural") || (&ident.sym == "Select") || (&ident.sym == "SelectOrdinal") {
            //     transform_icu_jsx_macro(&el);
            //     return el;
            // }
        }

        el
    }
}


/// An example plugin function with macro support.
/// `plugin_transform` macro interop pointers into deserialized structs, as well
/// as returning ptr back to host.
///
/// It is possible to opt out from macro by writing transform fn manually
/// if plugin need to handle low-level ptr directly via
/// `__transform_plugin_process_impl(
///     ast_ptr: *const u8, ast_ptr_len: i32,
///     unresolved_mark: u32, should_enable_comments_proxy: i32) ->
///     i32 /*  0 for success, fail otherwise.
///             Note this is only for internal pointer interop result,
///             not actual transform result */`
///
/// This requires manual handling of serialization / deserialization from ptrs.
/// Refer swc_plugin_macro to see how does it work internally.
#[plugin_transform]
pub fn process_transform(program: Program, _metadata: TransformPluginProgramMetadata) -> Program {
    program.fold_with(&mut TransformVisitor::default())
}

test!(
    Default::default(),
    |_| TransformVisitor::default(),
    should_not_touch_code_if_no_macro_import,
    // input
     r#"
     t`Refresh inbox`;
     "#,
    // output after transform
    r#"
    t`Refresh inbox`;
    "#
);

test!(
    Default::default(),
    |_| TransformVisitor::default(),
    should_not_touch_not_related_tagget_tpls,
    // input
     r#"
     import { t } from "@lingui/macro";

     b`Refresh inbox`;
     b(i18n)`Refresh inbox`;
     "#,
    // output after transform
    r#"
    b`Refresh inbox`;
    b(i18n)`Refresh inbox`;
    "#
);

test!(
    Default::default(),
    |_| TransformVisitor::default(),
    substitution_in_tpl_literal,
    // input
     r#"
     import { t } from "@lingui/macro";

     t`Refresh inbox`
     t`Refresh ${foo} inbox ${bar}`
     t`Refresh ${foo.bar} inbox ${bar}`
     t`Refresh ${expr()}`
     "#,
    // output after transform
    r#"
    import { i18n } from "@lingui/core";

    i18n._("Refresh inbox", {})
    i18n._("Refresh {foo} inbox {bar}", {foo: foo, bar: bar})
    i18n._("Refresh {0} inbox {bar}", {0: foo.bar, bar: bar})
    i18n._("Refresh {0}", {0: expr()})
    "#
);

test!(
    Default::default(),
    |_| TransformVisitor::default(),
    dedup_values_in_tpl_literal,
    // input
     r#"
     import { t } from "@lingui/macro";
     t`Refresh ${foo} inbox ${foo}`
     "#,
    // output after transform
    r#"
    import { i18n } from "@lingui/core";
    i18n._("Refresh {foo} inbox {foo}", {foo: foo})
    "#
);

test!(
    Default::default(),
    |_| TransformVisitor::default(),
    custom_i18n_passed,
    // input
     r#"
     import { t } from "@lingui/macro";
     import { custom_i18n } from "./i18n";

     t(custom_i18n)`Refresh inbox`
     t(custom_i18n)`Refresh ${foo} inbox ${bar}`
     t(custom_i18n)`Refresh ${foo.bar} inbox ${bar}`
     t(custom_i18n)`Refresh ${expr()}`
     "#,
    // output after transform
    r#"
    import { custom_i18n } from "./i18n";

    custom_i18n._("Refresh inbox", {})
    custom_i18n._("Refresh {foo} inbox {bar}", {foo: foo, bar: bar})
    custom_i18n._("Refresh {0} inbox {bar}", {0: foo.bar, bar: bar})
    custom_i18n._("Refresh {0}", {0: expr()})
    "#
);

test!(
    Default::default(),
    |_| TransformVisitor::default(),
    icu_functions,
     r#"
    import { plural, select, selectOrdinal } from "@lingui/macro";
    const messagePlural = plural(count, {
       one: '# Book',
       other: '# Books'
    })
    const messageSelect = select(gender, {
       male: 'he',
       female: 'she',
       other: 'they'
    })
    const messageSelectOrdinal = selectOrdinal(count, {
       one: '#st',
       two: '#nd',
       few: '#rd',
       other: '#th',
    })
     "#,
    r#"
    import { i18n } from "@lingui/core";
    const messagePlural = i18n._("{count, plural, one {# Book} other {# Books}}", {
      count: count
    });
    const messageSelect = i18n._("{gender, select, male {he} female {she} other {they}}", {
      gender: gender
    });
    const messageSelectOrdinal = i18n._("{count, selectOrdinal, one {#st} two {#nd} few {#rd} other {#th}}", {
      count: count
    });
    "#
);

test!(
    Default::default(),
    |_| TransformVisitor::default(),
    should_not_touch_non_lungui_fns,
     r#"
    import { plural } from "@lingui/macro";
    const messagePlural = customName(count, {
       one: '# Book',
       other: '# Books'
    })
     "#,
    r#"
   const messagePlural = customName(count, {
       one: '# Book',
       other: '# Books'
    })
    "#
);


test!(
    Default::default(),
    |_| TransformVisitor::default(),
    plural_with_placeholders,
     r#"
       import { plural } from "@lingui/macro";

       const message = plural(count, {
           one: `${name} has # friend`,
           other: `${name} has # friends`
        })
     "#,
    r#"
    import { i18n } from "@lingui/core";
    const message = i18n._("{count, plural, one {{name} has # friend} other {{name} has # friends}}", {
      count: count,
      name: name,
    })
    "#
);

test!(
    Default::default(),
    |_| TransformVisitor::default(),
    dedup_values_in_icu,
     r#"
       import { plural } from "@lingui/macro";

       const message = plural(count, {
           one: `${name} has ${count} friend`,
           other: `${name} has {count} friends`
        })
     "#,
    r#"
    import { i18n } from "@lingui/core";

    const message = i18n._("{count, plural, one {{name} has {count} friend} other {{name} has {count} friends}}", {
      count: count,
      name: name,
    })
    "#
);

test!(
       Syntax::Typescript(TsConfig {
        tsx: true,
        ..Default::default()
    }),
    |_| TransformVisitor::default(),
    simple_jsx,
     r#"
       import { Trans } from "@lingui/macro";
       const exp1 = <Custom>Refresh inbox</Custom>;
       const exp2 = <Trans>Refresh inbox</Trans>;
     "#,
    r#"
       import { Trans } from "@lingui/core";

       const exp1 = <Custom>Refresh inbox</Custom>;
       const exp2 = <Trans id={"Refresh inbox"} />
    "#
);

test!(
       Syntax::Typescript(TsConfig {
        tsx: true,
        ..Default::default()
    }),
    |_| TransformVisitor::default(),
    preserve_id_in_trans,
     r#"
       import { Trans } from "@lingui/macro";
       const exp2 = <Trans id="custom.id">Refresh inbox</Trans>;
     "#,
    r#"
       import { Trans } from "@lingui/core";
       const exp2 = <Trans message={"Refresh inbox"} id="custom.id"/>
    "#
);

test!(
       Syntax::Typescript(TsConfig {
        tsx: true,
        ..Default::default()
    }),
    |_| TransformVisitor::default(),
    jsx_interpolation,
     r#"
       import { Trans } from "@lingui/macro";
       <Trans>
          Property {props.name},
          function {random()},
          array {array[index]},
          constant {42},
          object {new Date()},
          everything {props.messages[index].value()}
        </Trans>;
     "#,
    r#"
       import { Trans } from "@lingui/core";
       <Trans id={"Property {0}, function {1}, array {2}, constant {3}, object {4}, everything {5}"} values={{
          0: props.name,
          1: random(),
          2: array[index],
          3: 42,
          4: new Date(),
          5: props.messages[index].value()
        }} />;
    "#
);

test!(
       Syntax::Typescript(TsConfig {
        tsx: true,
        ..Default::default()
    }),
    |_| TransformVisitor::default(),
    jsx_components_interpolation,
     r#"
       import { Trans } from "@lingui/macro";
       <Trans>
          Hello <strong>World!</strong><br />
          <p>
            My name is <a href="/about">{" "}
            <em>{name}</em></a>
          </p>
        </Trans>
     "#,
    r#"
    import { Trans } from "@lingui/core";
   <Trans id={"Hello <0>World!</0><1/><2>My name is <3> <4>{name}</4></3></2>"} values={{
      name: name,
    }} components={{
      0: <strong />,
      1: <br />,
      2: <p />,
      3: <a href="/about" />,
      4: <em />
    }} />;
    "#
);

test!(
       Syntax::Typescript(TsConfig {
        tsx: true,
        ..Default::default()
    }),
    |_| TransformVisitor::default(),
    jsx_values_dedup,
     r#"
       import { Trans } from "@lingui/macro";
       <Trans>
          Hello {foo} and {foo}
        </Trans>
     "#,
    r#"
       import { Trans } from "@lingui/core";
       <Trans id={"Hello {foo} and {foo}"} values={{
          foo: foo,
        }}/>;
    "#
);

test!(
       Syntax::Typescript(TsConfig {
        tsx: true,
        ..Default::default()
    }),
    |_| TransformVisitor::default(),
    should_not_add_extra_imports,
     r#"
       import { t } from "@lingui/macro";
       import { i18n, Trans } from "@lingui/core";

       t`Test`;
       <Trans>Test</Trans>;
     "#,
    r#"
       import { i18n, Trans } from "@lingui/core";

       i18n._("Test", {});
       <Trans id={"Test"}/>;
    "#
);

test!(
       Syntax::Typescript(TsConfig {
        tsx: true,
        ..Default::default()
    }),
    |_| TransformVisitor::default(),
    jsx_icu_nested,
     r#"
       import { Plural } from "@lingui/macro";

       <Trans>
       You have{" "}
          <Plural
           value={count}
           one="Message"
           other="Messages"
          />
      </Trans>
     "#,

    r#"
       import { Trans } from "@lingui/core";

       <Trans
           id={"You have {count, plural, one {Message} other {Messages}}"}
           values={{ count: count }}
        />
    "#
);

// test!(
//        Syntax::Typescript(TsConfig {
//         tsx: true,
//         ..Default::default()
//     }),
//     |_| TransformVisitor::default(),
//     jsx_icu_nested,
//      r#"
//        import { Plural } from "@lingui/macro";
//
//        <Trans>
//           <Plural
//            value={count}
//            one="Message"
//            other="Messages"
//           />
//       </Trans>
//      "#,
//
//     r#"
//        import { Trans } from "@lingui/core";
//
//        <Trans
//            id={"{count, plural, one {Message} other {Messages}}"}
//            values={{ count: count }}
//         />
//     "#
// );
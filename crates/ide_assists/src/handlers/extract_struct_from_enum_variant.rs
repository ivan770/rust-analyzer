use std::iter;

use either::Either;
use hir::{Module, ModuleDef, Name, Variant};
use ide_db::{
    defs::Definition,
    helpers::{
        insert_use::{insert_use, ImportScope, InsertUseConfig},
        mod_path_to_ast,
    },
    search::FileReference,
    RootDatabase,
};
use rustc_hash::FxHashSet;
use syntax::{
    algo::find_node_at_offset,
    ast::{self, make, AstNode, NameOwner, VisibilityOwner},
    ted, SyntaxNode, T,
};

use crate::{AssistContext, AssistId, AssistKind, Assists};

// Assist: extract_struct_from_enum_variant
//
// Extracts a struct from enum variant.
//
// ```
// enum A { $0One(u32, u32) }
// ```
// ->
// ```
// struct One(pub u32, pub u32);
//
// enum A { One(One) }
// ```
pub(crate) fn extract_struct_from_enum_variant(
    acc: &mut Assists,
    ctx: &AssistContext,
) -> Option<()> {
    let variant = ctx.find_node_at_offset::<ast::Variant>()?;
    let field_list = extract_field_list_if_applicable(&variant)?;

    let variant_name = variant.name()?;
    let variant_hir = ctx.sema.to_def(&variant)?;
    if existing_definition(ctx.db(), &variant_name, &variant_hir) {
        return None;
    }

    let enum_ast = variant.parent_enum();
    let enum_hir = ctx.sema.to_def(&enum_ast)?;
    let target = variant.syntax().text_range();
    acc.add(
        AssistId("extract_struct_from_enum_variant", AssistKind::RefactorRewrite),
        "Extract struct from enum variant",
        target,
        |builder| {
            let variant_hir_name = variant_hir.name(ctx.db());
            let enum_module_def = ModuleDef::from(enum_hir);
            let usages =
                Definition::ModuleDef(ModuleDef::Variant(variant_hir)).usages(&ctx.sema).all();

            let mut visited_modules_set = FxHashSet::default();
            let current_module = enum_hir.module(ctx.db());
            visited_modules_set.insert(current_module);
            // record file references of the file the def resides in, we only want to swap to the edited file in the builder once
            let mut def_file_references = None;
            for (file_id, references) in usages {
                if file_id == ctx.frange.file_id {
                    def_file_references = Some(references);
                    continue;
                }
                builder.edit_file(file_id);
                let source_file = builder.make_mut(ctx.sema.parse(file_id));
                let processed = process_references(
                    ctx,
                    &mut visited_modules_set,
                    source_file.syntax(),
                    &enum_module_def,
                    &variant_hir_name,
                    references,
                );
                processed.into_iter().for_each(|(path, node, import)| {
                    apply_references(ctx.config.insert_use, path, node, import)
                });
            }
            builder.edit_file(ctx.frange.file_id);
            let source_file = builder.make_mut(ctx.sema.parse(ctx.frange.file_id));
            let variant = builder.make_mut(variant.clone());
            if let Some(references) = def_file_references {
                let processed = process_references(
                    ctx,
                    &mut visited_modules_set,
                    source_file.syntax(),
                    &enum_module_def,
                    &variant_hir_name,
                    references,
                );
                processed.into_iter().for_each(|(path, node, import)| {
                    apply_references(ctx.config.insert_use, path, node, import)
                });
            }

            let def = create_struct_def(variant_name.clone(), &field_list, enum_ast.visibility());
            let start_offset = &variant.parent_enum().syntax().clone();
            ted::insert_raw(ted::Position::before(start_offset), def.syntax());
            ted::insert_raw(ted::Position::before(start_offset), &make::tokens::blank_line());

            update_variant(&variant);
        },
    )
}

fn extract_field_list_if_applicable(
    variant: &ast::Variant,
) -> Option<Either<ast::RecordFieldList, ast::TupleFieldList>> {
    match variant.kind() {
        ast::StructKind::Record(field_list) if field_list.fields().next().is_some() => {
            Some(Either::Left(field_list))
        }
        ast::StructKind::Tuple(field_list) if field_list.fields().count() > 1 => {
            Some(Either::Right(field_list))
        }
        _ => None,
    }
}

fn existing_definition(db: &RootDatabase, variant_name: &ast::Name, variant: &Variant) -> bool {
    variant
        .parent_enum(db)
        .module(db)
        .scope(db, None)
        .into_iter()
        .filter(|(_, def)| match def {
            // only check type-namespace
            hir::ScopeDef::ModuleDef(def) => matches!(
                def,
                ModuleDef::Module(_)
                    | ModuleDef::Adt(_)
                    | ModuleDef::Variant(_)
                    | ModuleDef::Trait(_)
                    | ModuleDef::TypeAlias(_)
                    | ModuleDef::BuiltinType(_)
            ),
            _ => false,
        })
        .any(|(name, _)| name.to_string() == variant_name.to_string())
}

fn create_struct_def(
    variant_name: ast::Name,
    field_list: &Either<ast::RecordFieldList, ast::TupleFieldList>,
    visibility: Option<ast::Visibility>,
) -> ast::Struct {
    let pub_vis = make::visibility_pub();

    let insert_pub = |node: &'_ SyntaxNode| {
        let pub_vis = pub_vis.clone_for_update();
        ted::insert(ted::Position::before(node), pub_vis.syntax());
    };

    // for fields without any existing visibility, use pub visibility
    let field_list = match field_list {
        Either::Left(field_list) => {
            let field_list = field_list.clone_for_update();

            field_list
                .fields()
                .filter(|field| field.visibility().is_none())
                .filter_map(|field| field.name())
                .for_each(|it| insert_pub(it.syntax()));

            field_list.into()
        }
        Either::Right(field_list) => {
            let field_list = field_list.clone_for_update();

            field_list
                .fields()
                .filter(|field| field.visibility().is_none())
                .filter_map(|field| field.ty())
                .for_each(|it| insert_pub(it.syntax()));

            field_list.into()
        }
    };

    make::struct_(visibility, variant_name, None, field_list).clone_for_update()
}

fn update_variant(variant: &ast::Variant) -> Option<()> {
    let name = variant.name()?;
    let tuple_field = make::tuple_field(None, make::ty(&name.text()));
    let replacement = make::variant(
        name,
        Some(ast::FieldList::TupleFieldList(make::tuple_field_list(iter::once(tuple_field)))),
    )
    .clone_for_update();
    ted::replace(variant.syntax(), replacement.syntax());
    Some(())
}

fn apply_references(
    insert_use_cfg: InsertUseConfig,
    segment: ast::PathSegment,
    node: SyntaxNode,
    import: Option<(ImportScope, hir::ModPath)>,
) {
    if let Some((scope, path)) = import {
        insert_use(&scope, mod_path_to_ast(&path), insert_use_cfg);
    }
    ted::insert_raw(
        ted::Position::before(segment.syntax()),
        make::path_from_text(&format!("{}", segment)).clone_for_update().syntax(),
    );
    ted::insert_raw(ted::Position::before(segment.syntax()), make::token(T!['(']));
    ted::insert_raw(ted::Position::after(&node), make::token(T![')']));
}

fn process_references(
    ctx: &AssistContext,
    visited_modules: &mut FxHashSet<Module>,
    source_file: &SyntaxNode,
    enum_module_def: &ModuleDef,
    variant_hir_name: &Name,
    refs: Vec<FileReference>,
) -> Vec<(ast::PathSegment, SyntaxNode, Option<(ImportScope, hir::ModPath)>)> {
    // we have to recollect here eagerly as we are about to edit the tree we need to calculate the changes
    // and corresponding nodes up front
    refs.into_iter()
        .flat_map(|reference| {
            let (segment, scope_node, module) =
                reference_to_node(&ctx.sema, source_file, reference)?;
            if !visited_modules.contains(&module) {
                let mod_path = module.find_use_path_prefixed(
                    ctx.sema.db,
                    *enum_module_def,
                    ctx.config.insert_use.prefix_kind,
                );
                if let Some(mut mod_path) = mod_path {
                    mod_path.pop_segment();
                    mod_path.push_segment(variant_hir_name.clone());
                    let scope = ImportScope::find_insert_use_container(&scope_node)?;
                    visited_modules.insert(module);
                    return Some((segment, scope_node, Some((scope, mod_path))));
                }
            }
            Some((segment, scope_node, None))
        })
        .collect()
}

fn reference_to_node(
    sema: &hir::Semantics<RootDatabase>,
    source_file: &SyntaxNode,
    reference: FileReference,
) -> Option<(ast::PathSegment, SyntaxNode, hir::Module)> {
    let offset = reference.range.start();
    if let Some(path_expr) = find_node_at_offset::<ast::PathExpr>(source_file, offset) {
        // tuple variant
        Some((path_expr.path()?.segment()?, path_expr.syntax().parent()?))
    } else if let Some(record_expr) = find_node_at_offset::<ast::RecordExpr>(source_file, offset) {
        // record variant
        Some((record_expr.path()?.segment()?, record_expr.syntax().clone()))
    } else {
        None
    }
    .and_then(|(segment, expr)| {
        let module = sema.scope(&expr).module()?;
        Some((segment, expr, module))
    })
}

#[cfg(test)]
mod tests {
    use ide_db::helpers::FamousDefs;

    use crate::tests::{check_assist, check_assist_not_applicable};

    use super::*;

    #[test]
    fn test_extract_struct_several_fields_tuple() {
        check_assist(
            extract_struct_from_enum_variant,
            "enum A { $0One(u32, u32) }",
            r#"struct One(pub u32, pub u32);

enum A { One(One) }"#,
        );
    }

    #[test]
    fn test_extract_struct_several_fields_named() {
        check_assist(
            extract_struct_from_enum_variant,
            "enum A { $0One { foo: u32, bar: u32 } }",
            r#"struct One{ pub foo: u32, pub bar: u32 }

enum A { One(One) }"#,
        );
    }

    #[test]
    fn test_extract_struct_one_field_named() {
        check_assist(
            extract_struct_from_enum_variant,
            "enum A { $0One { foo: u32 } }",
            r#"struct One{ pub foo: u32 }

enum A { One(One) }"#,
        );
    }

    #[test]
    fn test_extract_struct_keep_comments_and_attrs_one_field_named() {
        check_assist(
            extract_struct_from_enum_variant,
            r#"
enum A {
    $0One {
        // leading comment
        /// doc comment
        #[an_attr]
        foo: u32
        // trailing comment
    }
}"#,
            r#"
struct One{
        // leading comment
        /// doc comment
        #[an_attr]
        pub foo: u32
        // trailing comment
    }

enum A {
    One(One)
}"#,
        );
    }

    #[test]
    fn test_extract_struct_keep_comments_and_attrs_several_fields_named() {
        check_assist(
            extract_struct_from_enum_variant,
            r#"
enum A {
    $0One {
        // comment
        /// doc
        #[attr]
        foo: u32,
        // comment
        #[attr]
        /// doc
        bar: u32
    }
}"#,
            r#"
struct One{
        // comment
        /// doc
        #[attr]
        pub foo: u32,
        // comment
        #[attr]
        /// doc
        pub bar: u32
    }

enum A {
    One(One)
}"#,
        );
    }

    #[test]
    fn test_extract_struct_keep_comments_and_attrs_several_fields_tuple() {
        check_assist(
            extract_struct_from_enum_variant,
            "enum A { $0One(/* comment */ #[attr] u32, /* another */ u32 /* tail */) }",
            r#"
struct One(/* comment */ #[attr] pub u32, /* another */ pub u32 /* tail */);

enum A { One(One) }"#,
        );
    }

    #[test]
    fn test_extract_struct_keep_existing_visibility_named() {
        check_assist(
            extract_struct_from_enum_variant,
            "enum A { $0One{ pub a: u32, pub(crate) b: u32, pub(super) c: u32, d: u32 } }",
            r#"
struct One{ pub a: u32, pub(crate) b: u32, pub(super) c: u32, pub d: u32 }

enum A { One(One) }"#,
        );
    }

    #[test]
    fn test_extract_struct_keep_existing_visibility_tuple() {
        check_assist(
            extract_struct_from_enum_variant,
            "enum A { $0One(pub u32, pub(crate) u32, pub(super) u32, u32) }",
            r#"
struct One(pub u32, pub(crate) u32, pub(super) u32, pub u32);

enum A { One(One) }"#,
        );
    }

    #[test]
    fn test_extract_enum_variant_name_value_namespace() {
        check_assist(
            extract_struct_from_enum_variant,
            r#"const One: () = ();
enum A { $0One(u32, u32) }"#,
            r#"const One: () = ();
struct One(pub u32, pub u32);

enum A { One(One) }"#,
        );
    }

    #[test]
    fn test_extract_struct_pub_visibility() {
        check_assist(
            extract_struct_from_enum_variant,
            "pub enum A { $0One(u32, u32) }",
            r#"pub struct One(pub u32, pub u32);

pub enum A { One(One) }"#,
        );
    }

    #[test]
    fn test_extract_struct_with_complex_imports() {
        check_assist(
            extract_struct_from_enum_variant,
            r#"mod my_mod {
    fn another_fn() {
        let m = my_other_mod::MyEnum::MyField(1, 1);
    }

    pub mod my_other_mod {
        fn another_fn() {
            let m = MyEnum::MyField(1, 1);
        }

        pub enum MyEnum {
            $0MyField(u8, u8),
        }
    }
}

fn another_fn() {
    let m = my_mod::my_other_mod::MyEnum::MyField(1, 1);
}"#,
            r#"use my_mod::my_other_mod::MyField;

mod my_mod {
    use self::my_other_mod::MyField;

    fn another_fn() {
        let m = my_other_mod::MyEnum::MyField(MyField(1, 1));
    }

    pub mod my_other_mod {
        fn another_fn() {
            let m = MyEnum::MyField(MyField(1, 1));
        }

        pub struct MyField(pub u8, pub u8);

pub enum MyEnum {
            MyField(MyField),
        }
    }
}

fn another_fn() {
    let m = my_mod::my_other_mod::MyEnum::MyField(MyField(1, 1));
}"#,
        );
    }

    #[test]
    fn extract_record_fix_references() {
        check_assist(
            extract_struct_from_enum_variant,
            r#"
enum E {
    $0V { i: i32, j: i32 }
}

fn f() {
    let e = E::V { i: 9, j: 2 };
}
"#,
            r#"
struct V{ pub i: i32, pub j: i32 }

enum E {
    V(V)
}

fn f() {
    let e = E::V(V { i: 9, j: 2 });
}
"#,
        )
    }

    #[test]
    fn test_several_files() {
        check_assist(
            extract_struct_from_enum_variant,
            r#"
//- /main.rs
enum E {
    $0V(i32, i32)
}
mod foo;

//- /foo.rs
use crate::E;
fn f() {
    let e = E::V(9, 2);
}
"#,
            r#"
//- /main.rs
struct V(pub i32, pub i32);

enum E {
    V(V)
}
mod foo;

//- /foo.rs
use crate::{E, V};
fn f() {
    let e = E::V(V(9, 2));
}
"#,
        )
    }

    #[test]
    fn test_several_files_record() {
        check_assist(
            extract_struct_from_enum_variant,
            r#"
//- /main.rs
enum E {
    $0V { i: i32, j: i32 }
}
mod foo;

//- /foo.rs
use crate::E;
fn f() {
    let e = E::V { i: 9, j: 2 };
}
"#,
            r#"
//- /main.rs
struct V{ pub i: i32, pub j: i32 }

enum E {
    V(V)
}
mod foo;

//- /foo.rs
use crate::{E, V};
fn f() {
    let e = E::V(V { i: 9, j: 2 });
}
"#,
        )
    }

    #[test]
    fn test_extract_struct_record_nested_call_exp() {
        check_assist(
            extract_struct_from_enum_variant,
            r#"
enum A { $0One { a: u32, b: u32 } }

struct B(A);

fn foo() {
    let _ = B(A::One { a: 1, b: 2 });
}
"#,
            r#"
struct One{ pub a: u32, pub b: u32 }

enum A { One(One) }

struct B(A);

fn foo() {
    let _ = B(A::One(One { a: 1, b: 2 }));
}
"#,
        );
    }

    fn check_not_applicable(ra_fixture: &str) {
        let fixture =
            format!("//- /main.rs crate:main deps:core\n{}\n{}", ra_fixture, FamousDefs::FIXTURE);
        check_assist_not_applicable(extract_struct_from_enum_variant, &fixture)
    }

    #[test]
    fn test_extract_enum_not_applicable_for_element_with_no_fields() {
        check_not_applicable("enum A { $0One }");
    }

    #[test]
    fn test_extract_enum_not_applicable_if_struct_exists() {
        check_not_applicable(
            r#"struct One;
        enum A { $0One(u8, u32) }"#,
        );
    }

    #[test]
    fn test_extract_not_applicable_one_field() {
        check_not_applicable(r"enum A { $0One(u32) }");
    }

    #[test]
    fn test_extract_not_applicable_no_field_tuple() {
        check_not_applicable(r"enum A { $0None() }");
    }

    #[test]
    fn test_extract_not_applicable_no_field_named() {
        check_not_applicable(r"enum A { $0None {} }");
    }
}

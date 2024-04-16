//! # Struct-related libfuncs

use super::LibfuncHelper;
use crate::{
    error::Result, metadata::MetadataStorage, types::TypeBuilder, utils::ProgramRegistryExt,
};
use cairo_lang_sierra::{
    extensions::{
        core::{CoreLibfunc, CoreType},
        lib_func::SignatureOnlyConcreteLibfunc,
        structure::StructConcreteLibfunc,
        ConcreteLibfunc,
    },
    ids::ConcreteTypeId,
    program_registry::ProgramRegistry,
};
use melior::{
    dialect::{
        arith,
        llvm::{self, AllocaOptions, LoadStoreOptions},
    },
    ir::{
        attribute::{DenseI64ArrayAttribute, IntegerAttribute, TypeAttribute},
        r#type::IntegerType,
        Block, Location, Value,
    },
    Context,
};

/// Select and call the correct libfunc builder function from the selector.
pub fn build<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    selector: &StructConcreteLibfunc,
) -> Result<()> {
    match selector {
        StructConcreteLibfunc::Construct(info) => {
            build_construct(context, registry, entry, location, helper, metadata, info)
        }
        StructConcreteLibfunc::Deconstruct(info) => {
            build_deconstruct(context, registry, entry, location, helper, metadata, info)
        }
        StructConcreteLibfunc::SnapshotDeconstruct(info) => {
            build_deconstruct(context, registry, entry, location, helper, metadata, info)
        }
    }
}

/// Generate MLIR operations for the `struct_construct` libfunc.
pub fn build_construct<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureOnlyConcreteLibfunc,
) -> Result<()> {
    let mut fields = Vec::new();

    for (i, param) in info.param_signatures().iter().enumerate() {
        fields.push((param.ty.clone(), entry.argument(i).unwrap().into()));
    }

    let value = build_struct_value(
        context,
        registry,
        entry,
        location,
        helper,
        metadata,
        &info.branch_signatures()[0].vars[0].ty,
        &fields,
    )?;

    entry.append_operation(helper.br(0, &[value], location));

    Ok(())
}

/// Generate MLIR operations for the `struct_construct` libfunc.
#[allow(clippy::too_many_arguments)]
pub fn build_struct_value<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    struct_type: &ConcreteTypeId,
    fields: &[(ConcreteTypeId, Value<'ctx, 'this>)],
) -> Result<Value<'ctx, 'this>> {
    let (struct_ty, layout) =
        registry.build_type_with_layout(context, helper, registry, metadata, struct_type)?;

    let mut acc = entry.append_operation(llvm::undef(struct_ty, location));
    let mut is_memory_allocated = false;
    for (i, (field_ty, field_value)) in fields.iter().enumerate() {
        let type_info = registry.get_type(field_ty)?;

        let value = if type_info.is_memory_allocated(registry) {
            is_memory_allocated = true;
            entry
                .append_operation(llvm::load(
                    context,
                    *field_value,
                    type_info.build(context, helper, registry, metadata, field_ty)?,
                    location,
                    LoadStoreOptions::new().align(Some(IntegerAttribute::new(
                        IntegerType::new(context, 64).into(),
                        type_info.layout(registry)?.align() as i64,
                    ))),
                ))
                .result(0)?
                .into()
        } else {
            *field_value
        };

        acc = entry.append_operation(llvm::insert_value(
            context,
            acc.result(0)?.into(),
            DenseI64ArrayAttribute::new(context, &[i as _]),
            value,
            location,
        ));
    }

    if is_memory_allocated {
        let k1 = helper
            .init_block()
            .append_operation(arith::constant(
                context,
                IntegerAttribute::new(IntegerType::new(context, 64).into(), 1).into(),
                location,
            ))
            .result(0)?
            .into();
        let stack_ptr = helper
            .init_block()
            .append_operation(llvm::alloca(
                context,
                k1,
                llvm::r#type::opaque_pointer(context),
                location,
                AllocaOptions::new()
                    .align(Some(IntegerAttribute::new(
                        IntegerType::new(context, 64).into(),
                        layout.align() as i64,
                    )))
                    .elem_type(Some(TypeAttribute::new(struct_ty))),
            ))
            .result(0)?
            .into();

        entry.append_operation(llvm::store(
            context,
            acc.result(0)?.into(),
            stack_ptr,
            location,
            LoadStoreOptions::new().align(Some(IntegerAttribute::new(
                IntegerType::new(context, 64).into(),
                layout.align() as i64,
            ))),
        ));

        Ok(stack_ptr)
    } else {
        Ok(acc.result(0)?.into())
    }
}

/// Generate MLIR operations for the `struct_deconstruct` libfunc.
pub fn build_deconstruct<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureOnlyConcreteLibfunc,
) -> Result<()> {
    let type_info = registry.get_type(&info.param_signatures()[0].ty)?;
    let struct_ty = type_info.build(
        context,
        helper,
        registry,
        metadata,
        &info.param_signatures()[0].ty,
    )?;

    let container = if type_info.is_memory_allocated(registry) {
        entry
            .append_operation(llvm::load(
                context,
                entry.argument(0)?.into(),
                struct_ty,
                location,
                LoadStoreOptions::new().align(Some(IntegerAttribute::new(
                    IntegerType::new(context, 64).into(),
                    type_info.layout(registry)?.align() as i64,
                ))),
            ))
            .result(0)?
            .into()
    } else {
        entry.argument(0)?.into()
    };

    let mut fields = Vec::<Value>::with_capacity(info.branch_signatures()[0].vars.len());
    for (i, var_info) in info.branch_signatures()[0].vars.iter().enumerate() {
        let type_info = registry.get_type(&var_info.ty)?;
        let field_ty = type_info.build(context, helper, registry, metadata, &var_info.ty)?;

        let value = entry
            .append_operation(llvm::extract_value(
                context,
                container,
                DenseI64ArrayAttribute::new(context, &[i.try_into()?]),
                field_ty,
                location,
            ))
            .result(0)?
            .into();

        fields.push(if type_info.is_memory_allocated(registry) {
            let layout = type_info.layout(registry)?;

            let k1 = helper
                .init_block()
                .append_operation(arith::constant(
                    context,
                    IntegerAttribute::new(IntegerType::new(context, 64).into(), 1).into(),
                    location,
                ))
                .result(0)?
                .into();
            let stack_ptr = helper
                .init_block()
                .append_operation(llvm::alloca(
                    context,
                    k1,
                    llvm::r#type::opaque_pointer(context),
                    location,
                    AllocaOptions::new()
                        .align(Some(IntegerAttribute::new(
                            IntegerType::new(context, 64).into(),
                            layout.align() as i64,
                        )))
                        .elem_type(Some(TypeAttribute::new(field_ty))),
                ))
                .result(0)?
                .into();

            entry.append_operation(llvm::store(
                context,
                value,
                stack_ptr,
                location,
                LoadStoreOptions::new().align(Some(IntegerAttribute::new(
                    IntegerType::new(context, 64).into(),
                    layout.align() as i64,
                ))),
            ));

            stack_ptr
        } else {
            value
        });
    }

    entry.append_operation(helper.br(0, &fields, location));

    Ok(())
}

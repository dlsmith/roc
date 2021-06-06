use crate::debug_info_init;
use crate::llvm::bitcode::call_bitcode_fn;
use crate::llvm::build::Env;
use crate::llvm::build::{cast_block_of_memory_to_tag, complex_bitcast, FAST_CALL_CONV};
use crate::llvm::build_str;
use crate::llvm::convert::basic_type_from_layout;
use bumpalo::collections::Vec;
use inkwell::values::{
    BasicValue, BasicValueEnum, FunctionValue, IntValue, PointerValue, StructValue,
};
use roc_builtins::bitcode;
use roc_module::symbol::Symbol;
use roc_mono::layout::{Builtin, Layout, LayoutIds, UnionLayout};

#[derive(Clone, Debug)]
enum WhenRecursive<'a> {
    Unreachable,
    Loop(UnionLayout<'a>),
}

pub fn generic_hash<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    seed: IntValue<'ctx>,
    val: BasicValueEnum<'ctx>,
    layout: &Layout<'a>,
) -> IntValue<'ctx> {
    // NOTE: C and Zig use this value for their initial HashMap seed: 0xc70f6907
    build_hash_layout(
        env,
        layout_ids,
        seed,
        val,
        layout,
        WhenRecursive::Unreachable,
    )
}

fn build_hash_layout<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    seed: IntValue<'ctx>,
    val: BasicValueEnum<'ctx>,
    layout: &Layout<'a>,
    when_recursive: WhenRecursive<'a>,
) -> IntValue<'ctx> {
    match layout {
        Layout::Builtin(builtin) => {
            hash_builtin(env, layout_ids, seed, val, layout, builtin, when_recursive)
        }

        Layout::Struct(fields) => build_hash_struct(
            env,
            layout_ids,
            fields,
            when_recursive,
            seed,
            val.into_struct_value(),
        ),

        Layout::Union(union_layout) => {
            build_hash_tag(env, layout_ids, layout, union_layout, seed, val)
        }

        Layout::RecursivePointer => match when_recursive {
            WhenRecursive::Unreachable => {
                unreachable!("recursion pointers should never be hashed directly")
            }
            WhenRecursive::Loop(union_layout) => {
                let layout = Layout::Union(union_layout);

                let bt = basic_type_from_layout(env, &layout);

                // cast the i64 pointer to a pointer to block of memory
                let field_cast = env
                    .builder
                    .build_bitcast(val, bt, "i64_to_opaque")
                    .into_pointer_value();

                build_hash_tag(
                    env,
                    layout_ids,
                    &layout,
                    &union_layout,
                    seed,
                    field_cast.into(),
                )
            }
        },

        Layout::FunctionPointer(_, _) | Layout::Closure(_, _, _) => {
            unreachable!("the type system will guarantee these are never hashed")
        }
    }
}

fn append_hash_layout<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    seed: IntValue<'ctx>,
    val: BasicValueEnum<'ctx>,
    layout: &Layout<'a>,
    when_recursive: WhenRecursive<'a>,
) -> IntValue<'ctx> {
    build_hash_layout(env, layout_ids, seed, val, layout, when_recursive)
}

fn hash_builtin<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    seed: IntValue<'ctx>,
    val: BasicValueEnum<'ctx>,
    layout: &Layout<'a>,
    builtin: &Builtin<'a>,
    when_recursive: WhenRecursive<'a>,
) -> IntValue<'ctx> {
    let ptr_bytes = env.ptr_bytes;

    match builtin {
        Builtin::Int128
        | Builtin::Int64
        | Builtin::Int32
        | Builtin::Int16
        | Builtin::Int8
        | Builtin::Int1
        | Builtin::Float64
        | Builtin::Float32
        | Builtin::Float128
        | Builtin::Float16
        | Builtin::Usize => {
            let hash_bytes = store_and_use_as_u8_ptr(env, val, &layout);
            hash_bitcode_fn(env, seed, hash_bytes, layout.stack_size(ptr_bytes))
        }
        Builtin::Str => {
            // let zig deal with big vs small string
            call_bitcode_fn(
                env,
                &[seed.into(), build_str::str_to_i128(env, val).into()],
                &bitcode::DICT_HASH_STR,
            )
            .into_int_value()
        }
        Builtin::EmptyStr | Builtin::EmptyDict | Builtin::EmptyList | Builtin::EmptySet => {
            hash_empty_collection(seed)
        }

        Builtin::Dict(_, _) => {
            todo!("Implement hash for Dict")
        }
        Builtin::Set(_) => {
            todo!("Implement Hash for Set")
        }
        Builtin::List(element_layout) => build_hash_list(
            env,
            layout_ids,
            layout,
            element_layout,
            when_recursive,
            seed,
            val.into_struct_value(),
        ),
    }
}

fn build_hash_struct<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    field_layouts: &'a [Layout<'a>],
    when_recursive: WhenRecursive<'a>,
    seed: IntValue<'ctx>,
    value: StructValue<'ctx>,
) -> IntValue<'ctx> {
    let block = env.builder.get_insert_block().expect("to be in a function");
    let di_location = env.builder.get_current_debug_location().unwrap();

    let struct_layout = Layout::Struct(field_layouts);

    let symbol = Symbol::GENERIC_HASH;
    let fn_name = layout_ids
        .get(symbol, &struct_layout)
        .to_symbol_string(symbol, &env.interns);

    let function = match env.module.get_function(fn_name.as_str()) {
        Some(function_value) => function_value,
        None => {
            let seed_type = env.context.i64_type();

            let arg_type = basic_type_from_layout(env, &struct_layout);

            let function_value = crate::llvm::refcounting::build_header_help(
                env,
                &fn_name,
                seed_type.into(),
                &[seed_type.into(), arg_type],
            );

            build_hash_struct_help(
                env,
                layout_ids,
                function_value,
                when_recursive,
                field_layouts,
            );

            function_value
        }
    };

    env.builder.position_at_end(block);
    env.builder
        .set_current_debug_location(env.context, di_location);
    let call = env
        .builder
        .build_call(function, &[seed.into(), value.into()], "struct_hash");

    call.set_call_convention(FAST_CALL_CONV);

    call.try_as_basic_value().left().unwrap().into_int_value()
}

fn build_hash_struct_help<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    parent: FunctionValue<'ctx>,
    when_recursive: WhenRecursive<'a>,
    field_layouts: &[Layout<'a>],
) {
    let ctx = env.context;

    debug_info_init!(env, parent);

    // Add args to scope
    let mut it = parent.get_param_iter();
    let seed = it.next().unwrap().into_int_value();
    let value = it.next().unwrap().into_struct_value();

    seed.set_name(Symbol::ARG_1.ident_string(&env.interns));
    value.set_name(Symbol::ARG_2.ident_string(&env.interns));

    let entry = ctx.append_basic_block(parent, "entry");
    env.builder.position_at_end(entry);

    let result = hash_struct(env, layout_ids, seed, value, when_recursive, field_layouts);

    env.builder.build_return(Some(&result));
}

fn hash_struct<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    mut seed: IntValue<'ctx>,
    value: StructValue<'ctx>,
    when_recursive: WhenRecursive<'a>,
    field_layouts: &[Layout<'a>],
) -> IntValue<'ctx> {
    let ptr_bytes = env.ptr_bytes;

    let layout = Layout::Struct(field_layouts);

    // Optimization: if the bit representation of equal values is the same
    // just hash the bits. Caveat here is tags: e.g. `Nothing` in `Just a`
    // contains garbage bits after the tag (currently)
    if false {
        // this is a struct of only basic types, so we can just hash its bits
        let hash_bytes = store_and_use_as_u8_ptr(env, value.into(), &layout);
        hash_bitcode_fn(env, seed, hash_bytes, layout.stack_size(ptr_bytes))
    } else {
        for (index, field_layout) in field_layouts.iter().enumerate() {
            let field = env
                .builder
                .build_extract_value(value, index as u32, "eq_field")
                .unwrap();

            if let Layout::RecursivePointer = field_layout {
                match &when_recursive {
                    WhenRecursive::Unreachable => {
                        unreachable!("The current layout should not be recursive, but is")
                    }
                    WhenRecursive::Loop(union_layout) => {
                        let field_layout = Layout::Union(*union_layout);

                        let bt = basic_type_from_layout(env, &field_layout);

                        // cast the i64 pointer to a pointer to block of memory
                        let field_cast = env
                            .builder
                            .build_bitcast(field, bt, "i64_to_opaque")
                            .into_pointer_value();

                        seed = append_hash_layout(
                            env,
                            layout_ids,
                            seed,
                            field_cast.into(),
                            &field_layout,
                            when_recursive.clone(),
                        )
                    }
                }
            } else {
                seed = append_hash_layout(
                    env,
                    layout_ids,
                    seed,
                    field,
                    field_layout,
                    when_recursive.clone(),
                );
            }
        }
        seed
    }
}

fn build_hash_tag<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    layout: &Layout<'a>,
    union_layout: &UnionLayout<'a>,
    seed: IntValue<'ctx>,
    value: BasicValueEnum<'ctx>,
) -> IntValue<'ctx> {
    let block = env.builder.get_insert_block().expect("to be in a function");
    let di_location = env.builder.get_current_debug_location().unwrap();

    let symbol = Symbol::GENERIC_HASH;
    let fn_name = layout_ids
        .get(symbol, &layout)
        .to_symbol_string(symbol, &env.interns);

    let function = match env.module.get_function(fn_name.as_str()) {
        Some(function_value) => function_value,
        None => {
            let seed_type = env.context.i64_type();

            let arg_type = basic_type_from_layout(env, &layout);

            let function_value = crate::llvm::refcounting::build_header_help(
                env,
                &fn_name,
                seed_type.into(),
                &[seed_type.into(), arg_type],
            );

            build_hash_tag_help(env, layout_ids, function_value, union_layout);

            function_value
        }
    };

    env.builder.position_at_end(block);
    env.builder
        .set_current_debug_location(env.context, di_location);
    let call = env
        .builder
        .build_call(function, &[seed.into(), value], "struct_hash");

    call.set_call_convention(FAST_CALL_CONV);

    call.try_as_basic_value().left().unwrap().into_int_value()
}

fn build_hash_tag_help<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    parent: FunctionValue<'ctx>,
    union_layout: &UnionLayout<'a>,
) {
    let ctx = env.context;

    debug_info_init!(env, parent);

    // Add args to scope
    let mut it = parent.get_param_iter();
    let seed = it.next().unwrap().into_int_value();
    let value = it.next().unwrap();

    seed.set_name(Symbol::ARG_1.ident_string(&env.interns));
    value.set_name(Symbol::ARG_2.ident_string(&env.interns));

    let entry = ctx.append_basic_block(parent, "entry");
    env.builder.position_at_end(entry);

    let result = hash_tag(env, layout_ids, parent, seed, value, union_layout);

    env.builder.build_return(Some(&result));
}

fn hash_tag<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    parent: FunctionValue<'ctx>,
    seed: IntValue<'ctx>,
    tag: BasicValueEnum<'ctx>,
    union_layout: &UnionLayout<'a>,
) -> IntValue<'ctx> {
    use UnionLayout::*;

    let entry_block = env.builder.get_insert_block().unwrap();

    let merge_block = env.context.append_basic_block(parent, "merge_block");
    env.builder.position_at_end(merge_block);

    let merge_phi = env.builder.build_phi(env.context.i64_type(), "merge_hash");

    env.builder.position_at_end(entry_block);
    match union_layout {
        NonRecursive(tags) => {
            // SAFETY we know that non-recursive tags cannot be NULL
            let tag_id = nonrec_tag_id(env, tag.into_struct_value());

            let mut cases = Vec::with_capacity_in(tags.len(), env.arena);

            for (tag_id, field_layouts) in tags.iter().enumerate() {
                let block = env.context.append_basic_block(parent, "tag_id_modify");
                env.builder.position_at_end(block);

                // TODO drop tag id?
                let struct_layout = Layout::Struct(field_layouts);

                let wrapper_type = basic_type_from_layout(env, &struct_layout);
                debug_assert!(wrapper_type.is_struct_type());

                let as_struct =
                    cast_block_of_memory_to_tag(env.builder, tag.into_struct_value(), wrapper_type);

                let answer = build_hash_struct(
                    env,
                    layout_ids,
                    field_layouts,
                    WhenRecursive::Unreachable,
                    seed,
                    as_struct,
                );

                merge_phi.add_incoming(&[(&answer, block)]);
                env.builder.build_unconditional_branch(merge_block);

                cases.push((
                    env.context.i64_type().const_int(tag_id as u64, false),
                    block,
                ));
            }

            env.builder.position_at_end(entry_block);

            let default = cases.pop().unwrap().1;

            env.builder.build_switch(tag_id, default, &cases);
        }
        Recursive(tags) => {
            // SAFETY recursive tag unions are not NULL
            let tag_id = unsafe { rec_tag_id_unsafe(env, tag.into_pointer_value()) };

            let mut cases = Vec::with_capacity_in(tags.len(), env.arena);

            for (tag_id, field_layouts) in tags.iter().enumerate() {
                let block = env.context.append_basic_block(parent, "tag_id_modify");
                env.builder.position_at_end(block);

                let answer = hash_ptr_to_struct(
                    env,
                    layout_ids,
                    union_layout,
                    field_layouts,
                    seed,
                    tag.into_pointer_value(),
                );

                merge_phi.add_incoming(&[(&answer, block)]);
                env.builder.build_unconditional_branch(merge_block);

                cases.push((
                    env.context.i64_type().const_int(tag_id as u64, false),
                    block,
                ));
            }

            env.builder.position_at_end(entry_block);

            let default = cases.pop().unwrap().1;

            env.builder.build_switch(tag_id, default, &cases);
        }
        NullableUnwrapped { other_fields, .. } => {
            let tag = tag.into_pointer_value();
            let other_fields = &other_fields[1..];

            let is_null = env.builder.build_is_null(tag, "is_null");

            let hash_null_block = env.context.append_basic_block(parent, "hash_null_block");
            let hash_other_block = env.context.append_basic_block(parent, "hash_other_block");

            env.builder
                .build_conditional_branch(is_null, hash_null_block, hash_other_block);

            {
                env.builder.position_at_end(hash_null_block);

                let answer = hash_null(seed);

                merge_phi.add_incoming(&[(&answer, hash_null_block)]);
                env.builder.build_unconditional_branch(merge_block);
            }

            {
                env.builder.position_at_end(hash_other_block);

                let answer =
                    hash_ptr_to_struct(env, layout_ids, union_layout, other_fields, seed, tag);

                merge_phi.add_incoming(&[(&answer, hash_other_block)]);
                env.builder.build_unconditional_branch(merge_block);
            }
        }
        NullableWrapped { other_tags, .. } => {
            let tag = tag.into_pointer_value();

            let is_null = env.builder.build_is_null(tag, "is_null");

            let hash_null_block = env.context.append_basic_block(parent, "hash_null_block");
            let hash_other_block = env.context.append_basic_block(parent, "hash_other_block");

            env.builder
                .build_conditional_branch(is_null, hash_null_block, hash_other_block);

            {
                env.builder.position_at_end(hash_null_block);

                let answer = hash_null(seed);

                merge_phi.add_incoming(&[(&answer, hash_null_block)]);
                env.builder.build_unconditional_branch(merge_block);
            }

            {
                env.builder.position_at_end(hash_other_block);

                // SAFETY recursive tag unions are not NULL
                let tag_id = unsafe { rec_tag_id_unsafe(env, tag) };

                let mut cases = Vec::with_capacity_in(other_tags.len(), env.arena);

                for (tag_id, field_layouts) in other_tags.iter().enumerate() {
                    let block = env.context.append_basic_block(parent, "tag_id_modify");
                    env.builder.position_at_end(block);

                    let answer =
                        hash_ptr_to_struct(env, layout_ids, union_layout, field_layouts, seed, tag);

                    merge_phi.add_incoming(&[(&answer, block)]);
                    env.builder.build_unconditional_branch(merge_block);

                    cases.push((
                        env.context.i64_type().const_int(tag_id as u64, false),
                        block,
                    ));
                }

                env.builder.position_at_end(hash_other_block);

                let default = cases.pop().unwrap().1;

                env.builder.build_switch(tag_id, default, &cases);
            }
        }
        NonNullableUnwrapped(field_layouts) => {
            let answer = hash_ptr_to_struct(
                env,
                layout_ids,
                union_layout,
                field_layouts,
                seed,
                tag.into_pointer_value(),
            );

            merge_phi.add_incoming(&[(&answer, entry_block)]);
            env.builder.build_unconditional_branch(merge_block);
        }
    }

    env.builder.position_at_end(merge_block);

    merge_phi.as_basic_value().into_int_value()
}

fn build_hash_list<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    layout: &Layout<'a>,
    element_layout: &Layout<'a>,
    when_recursive: WhenRecursive<'a>,
    seed: IntValue<'ctx>,
    value: StructValue<'ctx>,
) -> IntValue<'ctx> {
    let block = env.builder.get_insert_block().expect("to be in a function");
    let di_location = env.builder.get_current_debug_location().unwrap();

    let symbol = Symbol::GENERIC_HASH;
    let fn_name = layout_ids
        .get(symbol, &layout)
        .to_symbol_string(symbol, &env.interns);

    let function = match env.module.get_function(fn_name.as_str()) {
        Some(function_value) => function_value,
        None => {
            let seed_type = env.context.i64_type();

            let arg_type = basic_type_from_layout(env, &layout);

            let function_value = crate::llvm::refcounting::build_header_help(
                env,
                &fn_name,
                seed_type.into(),
                &[seed_type.into(), arg_type],
            );

            build_hash_list_help(
                env,
                layout_ids,
                function_value,
                when_recursive,
                element_layout,
            );

            function_value
        }
    };

    env.builder.position_at_end(block);
    env.builder
        .set_current_debug_location(env.context, di_location);
    let call = env
        .builder
        .build_call(function, &[seed.into(), value.into()], "struct_hash");

    call.set_call_convention(FAST_CALL_CONV);

    call.try_as_basic_value().left().unwrap().into_int_value()
}

fn build_hash_list_help<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    parent: FunctionValue<'ctx>,
    when_recursive: WhenRecursive<'a>,
    element_layout: &Layout<'a>,
) {
    let ctx = env.context;

    debug_info_init!(env, parent);

    // Add args to scope
    let mut it = parent.get_param_iter();
    let seed = it.next().unwrap().into_int_value();
    let value = it.next().unwrap().into_struct_value();

    seed.set_name(Symbol::ARG_1.ident_string(&env.interns));
    value.set_name(Symbol::ARG_2.ident_string(&env.interns));

    let entry = ctx.append_basic_block(parent, "entry");
    env.builder.position_at_end(entry);

    let result = hash_list(
        env,
        layout_ids,
        parent,
        seed,
        value,
        when_recursive,
        element_layout,
    );

    env.builder.build_return(Some(&result));
}

fn hash_list<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    parent: FunctionValue<'ctx>,
    seed: IntValue<'ctx>,
    value: StructValue<'ctx>,
    when_recursive: WhenRecursive<'a>,
    element_layout: &Layout<'a>,
) -> IntValue<'ctx> {
    use crate::llvm::build_list::{incrementing_elem_loop, load_list};
    use inkwell::types::BasicType;

    // hash of a list is the hash of its elements
    let done_block = env.context.append_basic_block(parent, "done");
    let loop_block = env.context.append_basic_block(parent, "loop");

    let element_type = basic_type_from_layout(env, element_layout);
    let ptr_type = element_type.ptr_type(inkwell::AddressSpace::Generic);

    let (length, ptr) = load_list(env.builder, value, ptr_type);

    let result = env.builder.build_alloca(env.context.i64_type(), "result");
    env.builder.build_store(result, seed);

    let is_empty = env.builder.build_int_compare(
        inkwell::IntPredicate::EQ,
        length,
        env.ptr_int().const_zero(),
        "is_empty",
    );

    env.builder
        .build_conditional_branch(is_empty, done_block, loop_block);

    env.builder.position_at_end(loop_block);

    let loop_fn = |_index, element| {
        let seed = env
            .builder
            .build_load(result, "load_current")
            .into_int_value();

        let answer = append_hash_layout(
            env,
            layout_ids,
            seed,
            element,
            element_layout,
            when_recursive.clone(),
        );

        env.builder.build_store(result, answer);
    };

    incrementing_elem_loop(
        env.builder,
        env.context,
        parent,
        ptr,
        length,
        "current_index",
        loop_fn,
    );

    env.builder.build_unconditional_branch(done_block);

    env.builder.position_at_end(done_block);

    env.builder
        .build_load(result, "load_current")
        .into_int_value()
}

fn hash_null(seed: IntValue<'_>) -> IntValue<'_> {
    seed
}

fn hash_empty_collection(seed: IntValue<'_>) -> IntValue<'_> {
    seed
}

fn hash_ptr_to_struct<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    union_layout: &UnionLayout<'a>,
    field_layouts: &'a [Layout<'a>],
    seed: IntValue<'ctx>,
    tag: PointerValue<'ctx>,
) -> IntValue<'ctx> {
    use inkwell::types::BasicType;

    let struct_layout = Layout::Struct(field_layouts);

    let wrapper_type = basic_type_from_layout(env, &struct_layout);
    debug_assert!(wrapper_type.is_struct_type());

    // cast the opaque pointer to a pointer of the correct shape
    let struct_ptr = env
        .builder
        .build_bitcast(
            tag,
            wrapper_type.ptr_type(inkwell::AddressSpace::Generic),
            "opaque_to_correct",
        )
        .into_pointer_value();

    let struct_value = env
        .builder
        .build_load(struct_ptr, "load_struct1")
        .into_struct_value();

    build_hash_struct(
        env,
        layout_ids,
        field_layouts,
        WhenRecursive::Loop(*union_layout),
        seed,
        struct_value,
    )
}

fn store_and_use_as_u8_ptr<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    value: BasicValueEnum<'ctx>,
    layout: &Layout<'a>,
) -> PointerValue<'ctx> {
    let basic_type = basic_type_from_layout(env, &layout);
    let alloc = env.builder.build_alloca(basic_type, "store");
    env.builder.build_store(alloc, value);

    env.builder
        .build_bitcast(
            alloc,
            env.context
                .i8_type()
                .ptr_type(inkwell::AddressSpace::Generic),
            "as_u8_ptr",
        )
        .into_pointer_value()
}

fn hash_bitcode_fn<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    seed: IntValue<'ctx>,
    buffer: PointerValue<'ctx>,
    width: u32,
) -> IntValue<'ctx> {
    let num_bytes = env.context.i64_type().const_int(width as u64, false);

    call_bitcode_fn(
        env,
        &[seed.into(), buffer.into(), num_bytes.into()],
        &bitcode::DICT_HASH,
    )
    .into_int_value()
}

fn nonrec_tag_id<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    tag: StructValue<'ctx>,
) -> IntValue<'ctx> {
    complex_bitcast(
        env.builder,
        tag.into(),
        env.context.i64_type().into(),
        "load_tag_id",
    )
    .into_int_value()
}

unsafe fn rec_tag_id_unsafe<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    tag: PointerValue<'ctx>,
) -> IntValue<'ctx> {
    let ptr = env
        .builder
        .build_bitcast(
            tag,
            env.context
                .i64_type()
                .ptr_type(inkwell::AddressSpace::Generic),
            "cast_for_tag_id",
        )
        .into_pointer_value();

    env.builder.build_load(ptr, "load_tag_id").into_int_value()
}
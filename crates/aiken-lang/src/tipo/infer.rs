use super::{
    TypeInfo, ValueConstructor, ValueConstructorVariant,
    environment::{EntityKind, Environment},
    error::{Error, UnifyErrorSituation, Warning},
    expr::ExprTyper,
    hydrator::Hydrator,
};
use crate::{
    IdGenerator,
    ast::{
        Annotation, ArgBy, ArgName, ArgVia, DataType, Decorator, DecoratorKind, Definition,
        Function, ModuleConstant, ModuleKind, RecordConstructor, RecordConstructorArg, Tracing,
        TypeAlias, TypedArg, TypedDataType, TypedDefinition, TypedModule, TypedValidator,
        UntypedArg, UntypedDefinition, UntypedModule, UntypedPattern, UntypedValidator, Use,
        Validator,
    },
    expr::{TypedExpr, UntypedAssignmentKind, UntypedExpr},
    parser::token::Token,
    tipo::{Span, Type, TypeVar, expr::infer_function},
};
use std::{
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    ops::Deref,
    rc::Rc,
};

impl UntypedModule {
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)]
    pub fn infer(
        mut self,
        id_gen: &IdGenerator,
        kind: ModuleKind,
        package: &str,
        modules: &HashMap<String, TypeInfo>,
        tracing: Tracing,
        warnings: &mut Vec<Warning>,
        env: Option<&str>,
    ) -> Result<TypedModule, Error> {
        let module_name = self.name.clone();
        let docs = std::mem::take(&mut self.docs);
        let mut environment =
            Environment::new(id_gen.clone(), &module_name, &kind, modules, warnings, env);

        let mut type_names = HashMap::with_capacity(self.definitions.len());
        let mut value_names = HashMap::with_capacity(self.definitions.len());
        let mut hydrators = HashMap::with_capacity(self.definitions.len());

        // Register any modules, types, and values being imported
        // We process imports first so that anything imported can be referenced
        // anywhere in the module.
        for def in self.definitions() {
            environment.register_import(def)?;
        }

        // Register types so they can be used in constructors and functions
        // earlier in the module.
        environment.register_types(
            self.definitions.iter().collect(),
            &module_name,
            &mut hydrators,
            &mut type_names,
        )?;

        // Register values so they can be used in functions earlier in the module.
        for def in self.definitions() {
            environment.register_values(
                def,
                &module_name,
                &mut hydrators,
                &mut value_names,
                kind,
            )?;
        }

        // Infer the types of each definition in the module
        // We first infer all the constants so they can be used in functions defined
        // anywhere in the module.
        let mut definitions = Vec::with_capacity(self.definitions.len());
        let mut consts = vec![];
        let mut not_consts = vec![];

        for def in self.definitions().cloned() {
            match def {
                Definition::ModuleConstant { .. } => consts.push(def),
                Definition::Validator { .. } if kind.is_validator() => not_consts.push(def),
                Definition::Validator { .. } => (),
                Definition::Fn { .. }
                | Definition::Test { .. }
                | Definition::Benchmark { .. }
                | Definition::TypeAlias { .. }
                | Definition::DataType { .. }
                | Definition::Use { .. } => not_consts.push(def),
            }
        }

        for def in consts.into_iter().chain(not_consts) {
            let definition =
                infer_definition(def, &module_name, &mut hydrators, &mut environment, tracing)?;

            definitions.push(definition);
        }

        // Generalise functions now that the entire module has been inferred
        let definitions = definitions
            .into_iter()
            .map(|def| environment.generalise_definition(def, &module_name))
            .collect();

        // Generate warnings for unused items
        environment.warnings.retain(|warning| match warning {
            Warning::UnusedVariable { location, name } => !environment
                .validator_params
                .contains(&(name.to_string(), *location)),
            _ => true,
        });
        environment.convert_unused_to_warnings();

        // Remove private and imported types and values to create the public interface
        environment.module_values.retain(|_, info| info.public);

        // Ensure no exported values have private types in their type signature
        for value in environment.module_values.values() {
            if let Some(leaked) = value.tipo.find_private_type() {
                return Err(Error::PrivateTypeLeak {
                    location: value.variant.location(),
                    leaked_location: match &leaked {
                        Type::App { name, .. } => {
                            environment.module_types.get(name).map(|info| info.location)
                        }
                        _ => None,
                    },
                    leaked,
                });
            }
        }

        environment
            .module_types
            .retain(|_, info| info.public && info.module == module_name);

        let own_types = environment.module_types.keys().collect::<BTreeSet<_>>();

        environment
            .module_types_constructors
            .retain(|k, _| own_types.contains(k));

        environment
            .accessors
            .retain(|_, accessors| accessors.public);

        let Environment {
            module_types: types,
            module_types_constructors: types_constructors,
            module_values: values,
            accessors,
            annotations,
            ..
        } = environment;

        Ok(TypedModule {
            docs,
            name: module_name.clone(),
            definitions,
            kind,
            lines: self.lines,
            type_info: TypeInfo {
                name: module_name,
                types,
                types_constructors,
                values,
                accessors,
                annotations,
                kind,
                package: package.to_string(),
            },
        })
    }
}

#[allow(clippy::result_large_err)]
fn infer_definition(
    def: UntypedDefinition,
    module_name: &String,
    hydrators: &mut HashMap<String, Hydrator>,
    environment: &mut Environment<'_>,
    tracing: Tracing,
) -> Result<TypedDefinition, Error> {
    match def {
        Definition::Fn(f) => {
            let top_level_scope = environment.open_new_scope();
            let ret = Definition::Fn(infer_function(
                &f,
                module_name,
                hydrators,
                environment,
                tracing,
                &top_level_scope,
            )?);
            environment.close_scope(top_level_scope);
            Ok(ret)
        }

        Definition::Validator(Validator {
            doc,
            location,
            end_position,
            handlers,
            mut fallback,
            params,
            name,
        }) => {
            let params_length = params.len();

            let top_level_scope = environment.open_new_scope();

            let def = environment.in_new_scope(|environment| {
                let fallback_name = TypedValidator::handler_name(&name, &fallback.name);

                put_params_in_scope(&fallback_name, environment, &params);

                let mut typed_handlers = vec![];

                for mut handler in handlers {
                    let typed_fun = environment.in_new_scope(|environment| {
                        let temp_params = params.iter().cloned().chain(handler.arguments);
                        handler.arguments = temp_params.collect();

                        let handler_name = TypedValidator::handler_name(&name, &handler.name);

                        let old_name = handler.name;
                        handler.name = handler_name;

                        let mut typed_fun = infer_function(
                            &handler,
                            module_name,
                            hydrators,
                            environment,
                            tracing,
                            &top_level_scope,
                        )?;

                        typed_fun.name = old_name;

                        if !typed_fun.return_type.is_bool() {
                            return Err(Error::ValidatorMustReturnBool {
                                return_type: typed_fun.return_type.clone(),
                                location: typed_fun.location,
                            });
                        }

                        typed_fun.arguments.drain(0..params_length);

                        if !typed_fun.has_valid_purpose_name() {
                            return Err(Error::UnknownPurpose {
                                location: typed_fun
                                    .location
                                    .map(|start, _end| (start, start + typed_fun.name.len())),
                                available_purposes: TypedValidator::available_handler_names(),
                            });
                        }

                        if typed_fun.arguments.len() != typed_fun.validator_arity() {
                            return Err(Error::IncorrectValidatorArity {
                                count: typed_fun.arguments.len() as u32,
                                expected: typed_fun.validator_arity() as u32,
                                location: typed_fun.location,
                            });
                        }

                        if typed_fun.is_spend() && !typed_fun.arguments[0].tipo.is_option() {
                            return Err(Error::CouldNotUnify {
                                location: typed_fun.arguments[0].location,
                                expected: Type::option(typed_fun.arguments[0].tipo.clone()),
                                given: typed_fun.arguments[0].tipo.clone(),
                                situation: None,
                                rigid_type_names: Default::default(),
                            });
                        }

                        for arg in typed_fun.arguments.iter_mut() {
                            if arg.tipo.is_unbound() {
                                arg.tipo = Type::data();
                            }
                        }

                        Ok(typed_fun)
                    })?;

                    typed_handlers.push(typed_fun);
                }

                // NOTE: Duplicates are handled when registering handler names. So if we have N
                // typed handlers, they are different. The -1 represents takes out the fallback
                // handler name.
                let is_exhaustive =
                    typed_handlers.len() >= TypedValidator::available_handler_names().len() - 1;

                if is_exhaustive
                    && fallback != UntypedValidator::default_fallback(fallback.location)
                {
                    return Err(Error::UnexpectedValidatorFallback {
                        fallback: fallback.location,
                    });
                }

                let (typed_params, typed_fallback) = environment.in_new_scope(|environment| {
                    let temp_params = params.iter().cloned().chain(fallback.arguments);
                    fallback.arguments = temp_params.collect();

                    let old_name = fallback.name;
                    fallback.name = fallback_name;

                    let mut typed_fallback = infer_function(
                        &fallback,
                        module_name,
                        hydrators,
                        environment,
                        tracing,
                        &top_level_scope,
                    )?;

                    typed_fallback.name = old_name;

                    if !typed_fallback.return_type.is_bool() {
                        return Err(Error::ValidatorMustReturnBool {
                            return_type: typed_fallback.return_type.clone(),
                            location: typed_fallback.location,
                        });
                    }

                    let typed_params = typed_fallback
                        .arguments
                        .drain(0..params_length)
                        .map(|mut arg| {
                            if arg.tipo.is_unbound() {
                                arg.tipo = Type::data();
                            }

                            arg
                        })
                        .collect();

                    if typed_fallback.arguments.len() != 1 {
                        return Err(Error::IncorrectValidatorArity {
                            count: typed_fallback.arguments.len() as u32,
                            expected: 1,
                            location: typed_fallback.location,
                        });
                    }

                    for arg in typed_fallback.arguments.iter_mut() {
                        if arg.tipo.is_unbound() {
                            arg.tipo = Type::data();
                        }
                    }

                    Ok((typed_params, typed_fallback))
                })?;

                Ok(Definition::Validator(Validator {
                    doc,
                    end_position,
                    handlers: typed_handlers,
                    fallback: typed_fallback,
                    name,
                    location,
                    params: typed_params,
                }))
            })?;

            environment.close_scope(top_level_scope);

            Ok(def)
        }

        Definition::Test(f) => {
            let top_level_scope = environment.open_new_scope();
            let (typed_via, annotation) = match f.arguments.first() {
                Some(arg) => {
                    if f.arguments.len() > 1 {
                        return Err(Error::IncorrectTestArity {
                            count: f.arguments.len(),
                            location: f
                                .arguments
                                .get(1)
                                .expect("arguments.len() > 1")
                                .arg
                                .location,
                        });
                    }

                    extract_via_information(&f, arg, hydrators, environment, tracing, infer_fuzzer)
                        .map(|(typed_via, annotation)| (Some(typed_via), annotation))
                }
                None => Ok((None, None)),
            }?;

            let typed_f = infer_function(
                &f.into(),
                module_name,
                hydrators,
                environment,
                tracing,
                &top_level_scope,
            )?;

            let is_bool = environment.unify(
                typed_f.return_type.clone(),
                Type::bool(),
                typed_f.location,
                false,
            );

            let is_void = environment.unify(
                typed_f.return_type.clone(),
                Type::void(),
                typed_f.location,
                false,
            );

            environment.close_scope(top_level_scope);

            if is_bool.or(is_void).is_err() {
                return Err(Error::IllegalTestType {
                    location: typed_f.location,
                });
            }

            Ok(Definition::Test(Function {
                doc: typed_f.doc,
                location: typed_f.location,
                name: typed_f.name,
                public: typed_f.public,
                arguments: match typed_via {
                    Some((via, tipo)) => {
                        let arg = typed_f
                            .arguments
                            .first()
                            .expect("has exactly one argument")
                            .to_owned();
                        vec![ArgVia {
                            arg: TypedArg {
                                tipo,
                                annotation,
                                ..arg
                            },
                            via,
                        }]
                    }
                    None => vec![],
                },
                return_annotation: typed_f.return_annotation,
                return_type: typed_f.return_type,
                body: typed_f.body,
                on_test_failure: typed_f.on_test_failure,
                end_position: typed_f.end_position,
            }))
        }

        Definition::Benchmark(f) => {
            let top_level_scope = environment.open_new_scope();
            let err_incorrect_arity = || {
                Err(Error::IncorrectBenchmarkArity {
                    location: f
                        .location
                        .map(|start, end| (start + Token::Benchmark.to_string().len() + 1, end)),
                })
            };

            let (typed_via, annotation) = match f.arguments.first() {
                None => return err_incorrect_arity(),
                Some(arg) => {
                    if f.arguments.len() > 1 {
                        return err_incorrect_arity();
                    }

                    extract_via_information(&f, arg, hydrators, environment, tracing, infer_sampler)
                }
            }?;

            let typed_f = infer_function(
                &f.into(),
                module_name,
                hydrators,
                environment,
                tracing,
                &top_level_scope,
            )?;

            let arguments = {
                let arg = typed_f
                    .arguments
                    .first()
                    .expect("has exactly one argument")
                    .to_owned();

                vec![ArgVia {
                    arg: TypedArg {
                        tipo: typed_via.1,
                        annotation,
                        ..arg
                    },
                    via: typed_via.0,
                }]
            };

            environment.close_scope(top_level_scope);

            Ok(Definition::Benchmark(Function {
                doc: typed_f.doc,
                location: typed_f.location,
                name: typed_f.name,
                public: typed_f.public,
                arguments,
                return_annotation: typed_f.return_annotation,
                return_type: typed_f.return_type,
                body: typed_f.body,
                on_test_failure: typed_f.on_test_failure,
                end_position: typed_f.end_position,
            }))
        }

        Definition::TypeAlias(TypeAlias {
            doc,
            location,
            public,
            alias,
            parameters,
            annotation,
            tipo: _,
        }) => {
            let tipo = environment
                .get_type_constructor(&None, &alias, location)
                .expect("Could not find existing type for type alias")
                .tipo
                .clone();

            let typed_type_alias = TypeAlias {
                doc,
                location,
                public,
                alias,
                parameters,
                annotation,
                tipo,
            };

            Ok(Definition::TypeAlias(typed_type_alias))
        }

        Definition::DataType(DataType {
            doc,
            location,
            public,
            opaque,
            name,
            parameters,
            decorators,
            constructors: untyped_constructors,
            typed_parameters: _,
        }) => {
            let constructors = untyped_constructors
                .into_iter()
                .map(|constructor| {
                    let preregistered_fn = environment
                        .get_variable(&constructor.name)
                        .expect("Could not find preregistered type for function");

                    let preregistered_type = preregistered_fn.tipo.clone();

                    let args = preregistered_type.function_types().map_or(
                        Ok(vec![]),
                        |(args_types, _return_type)| {
                            constructor
                                .arguments
                                .into_iter()
                                .zip(&args_types)
                                .map(|(arg, t)| {
                                    if t.is_function() {
                                        return Err(Error::FunctionTypeInData {
                                            location: arg.location,
                                        });
                                    }

                                    if t.is_ml_result() {
                                        return Err(Error::IllegalTypeInData {
                                            location: arg.location,
                                            tipo: t.clone(),
                                        });
                                    }

                                    if t.contains_opaque() {
                                        let parent = environment
                                            .get_type_constructor_mut(&name, location)?;

                                        Rc::make_mut(&mut parent.tipo).set_opaque(true)
                                    }

                                    Ok(RecordConstructorArg {
                                        label: arg.label,
                                        annotation: arg.annotation,
                                        location: arg.location,
                                        doc: arg.doc,
                                        tipo: t.clone(),
                                    })
                                })
                                .collect()
                        },
                    )?;

                    Ok(RecordConstructor {
                        location: constructor.location,
                        name: constructor.name,
                        arguments: args,
                        decorators: constructor.decorators,
                        doc: constructor.doc,
                        sugar: constructor.sugar,
                    })
                })
                .collect::<Result<_, Error>>()?;

            let typed_parameters = environment
                .get_type_constructor(&None, &name, location)
                .expect("Could not find preregistered type constructor ")
                .parameters
                .clone();

            let typed_data = DataType {
                doc,
                location,
                public,
                opaque,
                name,
                parameters,
                constructors,
                decorators,
                typed_parameters,
            };

            for constr in &typed_data.constructors {
                for RecordConstructorArg {
                    tipo,
                    location,
                    doc: _,
                    label: _,
                    annotation: _,
                } in &constr.arguments
                {
                    if tipo.is_function() {
                        return Err(Error::FunctionTypeInData {
                            location: *location,
                        });
                    }

                    if tipo.is_ml_result() {
                        return Err(Error::IllegalTypeInData {
                            location: *location,
                            tipo: tipo.clone(),
                        });
                    }
                }
            }

            typed_data.check_decorators()?;

            Ok(Definition::DataType(typed_data))
        }

        Definition::Use(Use {
            location,
            module,
            as_name,
            unqualified,
            package: _,
        }) => {
            let module_info = environment.find_module(&module, location)?;

            Ok(Definition::Use(Use {
                location,
                module,
                as_name,
                unqualified,
                package: module_info.package.clone(),
            }))
        }

        Definition::ModuleConstant(ModuleConstant {
            doc,
            location,
            name,
            annotation,
            public,
            value,
        }) => {
            let typed_assignment = ExprTyper::new(environment, tracing).infer_assignment(
                UntypedPattern::Var {
                    location,
                    name: name.clone(),
                },
                value,
                UntypedAssignmentKind::Let { backpassing: false },
                &annotation,
                location,
            )?;

            // NOTE: The assignment above is only a convenient way to create the TypedExpression
            // that will be reduced at compile-time. We must increment its usage to not
            // automatically trigger a warning since we are virtually creating a block with a
            // single assignment that is then left unused.
            //
            // The usage of the constant is tracked through different means.
            environment.increment_usage(&name);

            let typed_expr = match typed_assignment {
                TypedExpr::Assignment { value, .. } => value,
                _ => unreachable!("infer_assignment inferred something else than an assignment?"),
            };

            let tipo = typed_expr.tipo();

            if tipo.is_function() && !tipo.is_monomorphic() {
                return Err(Error::GenericLeftAtBoundary { location });
            }

            let variant = ValueConstructor {
                public,
                variant: ValueConstructorVariant::ModuleConstant {
                    location,
                    name: name.to_owned(),
                    module: module_name.to_owned(),
                },
                tipo: tipo.clone(),
            };

            environment.insert_variable(name.clone(), variant.variant.clone(), tipo.clone());

            environment.insert_module_value(&name, variant);

            if !public {
                environment.init_usage(name.clone(), EntityKind::PrivateConstant, location);
            }

            Ok(Definition::ModuleConstant(ModuleConstant {
                doc,
                location,
                name,
                annotation,
                public,
                value: *typed_expr,
            }))
        }
    }
}

#[allow(clippy::result_large_err, clippy::type_complexity)]
fn extract_via_information<F>(
    f: &Function<(), UntypedExpr, ArgVia<UntypedArg, UntypedExpr>>,
    arg: &ArgVia<UntypedArg, UntypedExpr>,
    hydrators: &mut HashMap<String, Hydrator>,
    environment: &mut Environment<'_>,
    tracing: Tracing,
    infer_via: F,
) -> Result<((TypedExpr, Rc<Type>), Option<Annotation>), Error>
where
    F: FnOnce(&mut Environment<'_>, Option<Rc<Type>>, &Rc<Type>, &Span) -> Result<Rc<Type>, Error>,
{
    let typed_via = ExprTyper::new(environment, tracing).infer(arg.via.clone())?;

    let hydrator: &mut Hydrator = hydrators.get_mut(&f.name).unwrap();

    let provided_inner_type = arg
        .arg
        .annotation
        .as_ref()
        .map(|ann| hydrator.type_from_annotation(ann, environment))
        .transpose()?;

    let inferred_inner_type = infer_via(
        environment,
        provided_inner_type.clone(),
        &typed_via.tipo(),
        &arg.via.location(),
    )?;

    // Ensure that the annotation, if any, matches the type inferred from the
    // Fuzzer.
    if let Some(provided_inner_type) = provided_inner_type {
        environment
            .unify(
                inferred_inner_type.clone(),
                provided_inner_type.clone(),
                arg.via.location(),
                false,
            )
            .map_err(|err| {
                err.with_unify_error_situation(UnifyErrorSituation::FuzzerAnnotationMismatch)
            })?;
    }

    // Replace the pre-registered type for the test function, to allow inferring
    // the function body with the right type arguments.
    let scope = environment
        .scope
        .get_mut(&f.name)
        .expect("Could not find preregistered type for test");
    if let Type::Fn {
        ret,
        alias,
        args: _,
    } = scope.tipo.as_ref()
    {
        scope.tipo = Rc::new(Type::Fn {
            ret: ret.clone(),
            args: vec![inferred_inner_type.clone()],
            alias: alias.clone(),
        })
    }

    Ok(((typed_via, inferred_inner_type), arg.arg.annotation.clone()))
}

#[allow(clippy::result_large_err)]
fn infer_fuzzer(
    environment: &mut Environment<'_>,
    expected_inner_type: Option<Rc<Type>>,
    tipo: &Rc<Type>,
    location: &Span,
) -> Result<Rc<Type>, Error> {
    let could_not_unify = || Error::CouldNotUnify {
        location: *location,
        expected: Type::fuzzer(
            expected_inner_type
                .clone()
                .unwrap_or_else(|| Type::generic_var(0)),
        ),
        given: tipo.clone(),
        situation: None,
        rigid_type_names: HashMap::new(),
    };

    match tipo.borrow() {
        Type::Fn {
            ret,
            args: _,
            alias: _,
        } => match ret.borrow() {
            Type::App {
                module,
                name,
                args,
                public: _,
                contains_opaque: _,
                alias: _,
            } if module.is_empty() && name == "Option" && args.len() == 1 => {
                match args.first().expect("args.len() == 1").borrow() {
                    Type::Tuple { elems, .. } if elems.len() == 2 => {
                        let wrapped = elems.get(1).expect("Tuple has two elements");

                        // Disallow generics and functions as fuzzer targets. Only allow plain
                        // concrete types.
                        is_valid_fuzzer(wrapped, location)?;

                        // NOTE: Although we've drilled through the Fuzzer structure to get here,
                        // we still need to enforce that:
                        //
                        // 1. The Fuzzer is a function with a single argument of type PRNG
                        // 2. It returns not only a wrapped type, but also a new PRNG
                        //
                        // All-in-all, we could bundle those verification through the
                        // `infer_fuzzer` function, but instead, we can also just piggyback on
                        // `unify` now that we have figured out the type carried by the fuzzer.
                        environment.unify(
                            tipo.clone(),
                            Type::fuzzer(wrapped.clone()),
                            *location,
                            false,
                        )?;

                        Ok(wrapped.clone())
                    }
                    _ => Err(could_not_unify()),
                }
            }
            _ => Err(could_not_unify()),
        },

        Type::Var { tipo, alias } => match &*tipo.deref().borrow() {
            TypeVar::Link { tipo } => infer_fuzzer(
                environment,
                expected_inner_type,
                &Type::with_alias(tipo.clone(), alias.clone()),
                location,
            ),
            _ => Err(Error::GenericLeftAtBoundary {
                location: *location,
            }),
        },

        Type::App { .. } | Type::Tuple { .. } | Type::Pair { .. } => Err(could_not_unify()),
    }
}

#[allow(clippy::result_large_err)]
fn infer_sampler(
    environment: &mut Environment<'_>,
    expected_inner_type: Option<Rc<Type>>,
    tipo: &Rc<Type>,
    location: &Span,
) -> Result<Rc<Type>, Error> {
    let could_not_unify = || Error::CouldNotUnify {
        location: *location,
        expected: Type::sampler(
            expected_inner_type
                .clone()
                .unwrap_or_else(|| Type::generic_var(0)),
        ),
        given: tipo.clone(),
        situation: None,
        rigid_type_names: HashMap::new(),
    };

    match tipo.borrow() {
        Type::Fn {
            ret,
            args,
            alias: _,
        } => {
            if args.len() == 1 && args[0].is_int() {
                infer_fuzzer(environment, expected_inner_type, ret, &Span::empty())
            } else {
                Err(could_not_unify())
            }
        }

        Type::Var { tipo, alias } => match &*tipo.deref().borrow() {
            TypeVar::Link { tipo } => infer_sampler(
                environment,
                expected_inner_type,
                &Type::with_alias(tipo.clone(), alias.clone()),
                location,
            ),
            _ => Err(Error::GenericLeftAtBoundary {
                location: *location,
            }),
        },

        Type::App { .. } | Type::Tuple { .. } | Type::Pair { .. } => Err(could_not_unify()),
    }
}

#[allow(clippy::result_large_err)]
fn is_valid_fuzzer(tipo: &Type, location: &Span) -> Result<(), Error> {
    match tipo {
        Type::App {
            name: _name,
            module: _module,
            args,
            public: _,
            contains_opaque: _,
            alias: _,
        } => args
            .iter()
            .try_for_each(|arg| is_valid_fuzzer(arg, location)),

        Type::Tuple { elems, alias: _ } => elems
            .iter()
            .try_for_each(|arg| is_valid_fuzzer(arg, location)),

        Type::Var { tipo, alias: _ } => match &*tipo.deref().borrow() {
            TypeVar::Link { tipo } => is_valid_fuzzer(tipo, location),
            _ => Err(Error::GenericLeftAtBoundary {
                location: *location,
            }),
        },
        Type::Fn { .. } => Err(Error::IllegalTypeInData {
            location: *location,
            tipo: Rc::new(tipo.clone()),
        }),
        Type::Pair { fst, snd, .. } => {
            is_valid_fuzzer(fst, location)?;
            is_valid_fuzzer(snd, location)?;
            Ok(())
        }
    }
}

fn put_params_in_scope<'a>(
    name: &'_ str,
    environment: &'a mut Environment,
    params: &'a [UntypedArg],
) {
    let preregistered_fn = environment
        .get_variable(name)
        .expect("Could not find preregistered type for function");

    let preregistered_type = preregistered_fn.tipo.clone();

    let (args_types, _return_type) = preregistered_type
        .function_types()
        .expect("Preregistered type for fn was not a fn");

    for (ix, (arg, t)) in params
        .iter()
        .zip(args_types[0..params.len()].iter())
        .enumerate()
    {
        match arg.arg_name(ix) {
            ArgName::Named {
                name,
                label: _,
                location: _,
            } if arg.is_validator_param => {
                environment.insert_variable(
                    name.to_string(),
                    ValueConstructorVariant::LocalVariable {
                        location: arg.location,
                    },
                    t.clone(),
                );

                if let ArgBy::ByPattern(ref pattern) = arg.by {
                    pattern.collect_identifiers(&mut |identifier| {
                        environment.validator_params.insert(identifier);
                    })
                }

                environment.init_usage(name, EntityKind::Variable, arg.location);
            }
            ArgName::Named { .. } | ArgName::Discarded { .. } => (),
        };
    }
}

#[derive(Debug, PartialEq)]
pub enum DecoratorContext {
    Record,
    Enum,
    Constructor,
}

impl fmt::Display for DecoratorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecoratorContext::Record => write!(f, "record"),
            DecoratorContext::Enum => write!(f, "enum"),
            DecoratorContext::Constructor => write!(f, "constructor"),
        }
    }
}

impl TypedDataType {
    #[allow(clippy::result_large_err)]
    fn check_decorators(&self) -> Result<(), Error> {
        // First determine if this is a record or enum type
        let is_enum = self.constructors.len() > 1;

        let context = if is_enum {
            DecoratorContext::Enum
        } else {
            DecoratorContext::Record
        };

        validate_decorators_in_context(&self.decorators, context, None)?;

        let mut seen = BTreeMap::new();

        // Validate constructor decorators
        for (index, constructor) in self.constructors.iter().enumerate() {
            validate_decorators_in_context(
                &constructor.decorators,
                DecoratorContext::Constructor,
                None,
            )?;

            let (tag, location) = constructor
                .decorators
                .iter()
                .find_map(|decorator| {
                    if let DecoratorKind::Tag { value, .. } = &decorator.kind {
                        Some((value.parse().unwrap(), &decorator.location))
                    } else {
                        None
                    }
                })
                .unwrap_or((index, &constructor.location));

            if let Some(first) = seen.insert(tag, location) {
                return Err(Error::DecoratorTagOverlap {
                    tag,
                    first: *first,
                    second: *location,
                });
            }
        }

        Ok(())
    }
}

#[allow(clippy::result_large_err)]
fn validate_decorators_in_context(
    decorators: &[Decorator],
    context: DecoratorContext,
    tipo: Option<&Type>,
) -> Result<(), Error> {
    // Check for conflicts between decorators
    for (i, d1) in decorators.iter().enumerate() {
        // Validate context
        if !d1.kind.allowed_contexts().contains(&context) {
            return Err(Error::DecoratorValidation {
                location: d1.location,
                message: format!("this decorator not allowed in a {context} context"),
            });
        }

        // Validate type constraints if applicable
        if let Some(t) = tipo {
            d1.kind.validate_type(&context, t, d1.location)?;
        }

        // Check for conflicts with other decorators
        for d2 in decorators.iter().skip(i + 1) {
            if d1.kind.conflicts_with(&d2.kind) {
                return Err(Error::ConflictingDecorators {
                    location: d1.location,
                    conflicting_location: d2.location,
                });
            }
        }
    }

    Ok(())
}

impl DecoratorKind {
    fn allowed_contexts(&self) -> &[DecoratorContext] {
        match self {
            DecoratorKind::Tag { .. } => &[DecoratorContext::Record, DecoratorContext::Constructor],
            DecoratorKind::List => &[DecoratorContext::Record],
        }
    }

    #[allow(clippy::result_large_err)]
    fn validate_type(
        &self,
        _context: &DecoratorContext,
        _tipo: &Type,
        _loc: Span,
    ) -> Result<(), Error> {
        match self {
            DecoratorKind::Tag { .. } => Ok(()),
            DecoratorKind::List => Ok(()),
        }
    }

    fn conflicts_with(&self, other: &DecoratorKind) -> bool {
        match (self, other) {
            (DecoratorKind::Tag { .. }, DecoratorKind::List) => true,
            (DecoratorKind::Tag { .. }, DecoratorKind::Tag { .. }) => true,
            (DecoratorKind::List, DecoratorKind::Tag { .. }) => true,
            (DecoratorKind::List, DecoratorKind::List) => true,
        }
    }
}

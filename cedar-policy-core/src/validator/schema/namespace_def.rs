/*
 * Copyright Cedar Contributors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! This module contains the definition of `ValidatorNamespaceDef` and of types
//! it relies on

use std::collections::{hash_map::Entry, BTreeMap, HashMap, HashSet};

use crate::{
    ast::{
        EntityAttrEvaluationError, EntityType, EntityUID, InternalName, Name, PartialValue,
        UnreservedId,
    },
    entities::{json::err::JsonDeserializationErrorContext, CedarValueJson},
    evaluator::RestrictedEvaluator,
    extensions::Extensions,
    fuzzy_match::fuzzy_search,
    parser::{AsLocRef, IntoMaybeLoc, Loc, MaybeLoc},
};
use itertools::Itertools;
use nonempty::{nonempty, NonEmpty};
use smol_str::{SmolStr, ToSmolStr};

use super::{internal_name_to_entity_type, AllDefs, ValidatorApplySpec, ValidatorType};
use crate::validator::{
    err::{schema_errors::*, SchemaError},
    json_schema::{self, CommonTypeId, EntityTypeKind},
    partition_nonempty::PartitionNonEmpty,
    types::{AttributeType, Attributes, OpenTag, Type},
    ActionBehavior, ConditionalName, RawName, ReferenceType,
};

/// A single namespace definition from the schema JSON or Cedar syntax,
/// processed into a form which is closer to that used by the validator.
/// The processing includes detection of some errors, for example, parse errors
/// in entity/common type names or entity/common types which are declared
/// multiple times.
///
/// In this representation, there may still be references to undeclared
/// entity/common types, because any entity/common type may be declared in a
/// different fragment that will only be known about when building the complete
/// [`crate::validator::ValidatorSchema`].
///
/// The parameter `N` is the type of entity type names and common type names in
/// attributes/parents fields in this [`ValidatorNamespaceDef`], including
/// recursively. (It doesn't affect the type of common and entity type names
/// _that are being declared here_, which are already fully-qualified in this
/// representation. It only affects the type of common and entity type
/// _references_.)
/// For example:
/// - `N` = [`ConditionalName`]: References to entity/common types are not
///   yet fully qualified/disambiguated
/// - `N` = [`InternalName`]: All references to entity/common types have been
///   resolved into fully-qualified [`InternalName`]s
///
/// `A` is like `N`, but `A` governs typenames in `appliesTo` fields, while
/// `N` governs all other type references.
#[derive(Debug, Clone)]
pub struct ValidatorNamespaceDef<N, A> {
    /// The (fully-qualified) name of the namespace this is a definition of, or
    /// `None` if this is a definition for the empty namespace.
    ///
    /// This is informational only; it does not change the semantics of any
    /// definition in `common_types`, `entity_types`, or `actions`. All
    /// entity/common type names in `common_types`, `entity_types`, and
    /// `actions` are already either fully qualified/disambiguated, or stored in
    /// [`ConditionalName`] format which does not require referencing the
    /// implicit `namespace` directly any longer.
    /// This `namespace` field is used only in tests and by the `cedar_policy`
    /// function `SchemaFragment::namespaces()`.
    namespace: Option<InternalName>,
    /// Common type definitions, which can be used to define entity
    /// type attributes, action contexts, and other common types.
    pub(super) common_types: CommonTypeDefs<N>,
    /// Entity type declarations.
    pub(super) entity_types: EntityTypesDef<N>,
    /// Action declarations.
    pub(super) actions: ActionsDef<N, A>,
    #[cfg(feature = "extended-schema")]
    pub(super) loc: MaybeLoc,
}

impl<N, A> ValidatorNamespaceDef<N, A> {
    /// Get the fully-qualified [`InternalName`]s of all entity types declared
    /// in this [`ValidatorNamespaceDef`].
    pub fn all_declared_entity_type_names(&self) -> impl Iterator<Item = &InternalName> {
        self.entity_types
            .defs
            .keys()
            .map(|ety| ety.as_ref().as_ref())
    }

    /// Get the fully-qualified [`InternalName`]s of all common types declared
    /// in this [`ValidatorNamespaceDef`].
    pub fn all_declared_common_type_names(&self) -> impl Iterator<Item = &InternalName> {
        self.common_types.defs.keys()
    }

    /// Get the fully-qualified [`EntityUID`]s of all actions declared in this
    /// [`ValidatorNamespaceDef`].
    pub fn all_declared_action_names(&self) -> impl Iterator<Item = &EntityUID> {
        self.actions.actions.keys()
    }

    /// The fully-qualified [`InternalName`] of the namespace this is a definition of.
    /// `None` indicates this definition is for the empty namespace.
    pub fn namespace(&self) -> Option<&InternalName> {
        self.namespace.as_ref()
    }
}

impl ValidatorNamespaceDef<ConditionalName, ConditionalName> {
    /// Construct a new [`ValidatorNamespaceDef<ConditionalName>`] from the raw [`json_schema::NamespaceDefinition`]
    pub fn from_namespace_definition(
        namespace: Option<InternalName>,
        namespace_def: json_schema::NamespaceDefinition<RawName>,
        action_behavior: ActionBehavior,
        extensions: &Extensions<'_>,
    ) -> crate::validator::err::Result<ValidatorNamespaceDef<ConditionalName, ConditionalName>>
    {
        // Return early with an error if actions cannot be in groups or have
        // attributes, but the schema contains action groups or attributes.
        Self::check_action_behavior(&namespace_def, action_behavior)?;

        // Convert the common types, actions and entity types from the schema
        // file into the representation used by the validator.
        let common_types =
            CommonTypeDefs::from_raw_common_types(namespace_def.common_types, namespace.as_ref())?;
        let actions =
            ActionsDef::from_raw_actions(namespace_def.actions, namespace.as_ref(), extensions)?;
        let entity_types =
            EntityTypesDef::from_raw_entity_types(namespace_def.entity_types, namespace.as_ref())?;

        Ok(ValidatorNamespaceDef {
            namespace,
            common_types,
            entity_types,
            actions,
            #[cfg(feature = "extended-schema")]
            loc: namespace_def.loc,
        })
    }

    /// Construct a new [`ValidatorNamespaceDef<ConditionalName>`] containing
    /// only the given common-type definitions, which are already given in
    /// terms of [`ConditionalName`]s.
    pub fn from_common_type_defs(
        namespace: Option<InternalName>,
        defs: HashMap<UnreservedId, json_schema::Type<ConditionalName>>,
    ) -> crate::validator::err::Result<ValidatorNamespaceDef<ConditionalName, ConditionalName>>
    {
        let common_types = CommonTypeDefs::from_conditionalname_typedefs(defs, namespace.as_ref())?;
        Ok(ValidatorNamespaceDef {
            namespace,
            common_types,
            entity_types: EntityTypesDef::new(),
            actions: ActionsDef::new(),
            #[cfg(feature = "extended-schema")]
            loc: None,
        })
    }

    /// Construct a new [`ValidatorNamespaceDef<ConditionalName>`] containing
    /// only a single given common-type definition, which is already given in
    /// terms of [`ConditionalName`]s.
    ///
    /// Unlike `from_common_type_defs()`, this function cannot fail, because
    /// there is only one def so it cannot have a name collision with itself
    pub fn from_common_type_def(
        namespace: Option<InternalName>,
        def: (UnreservedId, json_schema::Type<ConditionalName>),
    ) -> ValidatorNamespaceDef<ConditionalName, ConditionalName> {
        let common_types = CommonTypeDefs::from_conditionalname_typedef(def, namespace.as_ref());
        ValidatorNamespaceDef {
            namespace,
            common_types,
            entity_types: EntityTypesDef::new(),
            actions: ActionsDef::new(),
            #[cfg(feature = "extended-schema")]
            loc: None,
        }
    }

    /// Convert this [`ValidatorNamespaceDef<ConditionalName>`] into a
    /// [`ValidatorNamespaceDef<InternalName>`] by fully-qualifying all
    /// typenames that appear anywhere in any definitions.
    ///
    /// `all_defs` needs to contain the full set of all fully-qualified typenames
    /// and actions that are defined in the schema (in all schema fragments).
    pub fn fully_qualify_type_references(
        self,
        all_defs: &AllDefs,
    ) -> Result<ValidatorNamespaceDef<InternalName, EntityType>, SchemaError> {
        match (
            self.common_types.fully_qualify_type_references(all_defs),
            self.entity_types.fully_qualify_type_references(all_defs),
            self.actions.fully_qualify_type_references(all_defs),
        ) {
            (Ok(common_types), Ok(entity_types), Ok(actions)) => Ok(ValidatorNamespaceDef {
                namespace: self.namespace,
                common_types,
                entity_types,
                actions,
                #[cfg(feature = "extended-schema")]
                loc: self.loc,
            }),
            (res1, res2, res3) => {
                // PANIC SAFETY: at least one of the results is `Err`, so the input to `NonEmpty::collect()` cannot be an empty iterator
                #[allow(clippy::expect_used)]
                let errs = NonEmpty::collect(
                    res1.err()
                        .into_iter()
                        .map(SchemaError::from)
                        .chain(res2.err().map(SchemaError::from))
                        .chain(res3.err()),
                )
                .expect("there must be an error");
                Err(SchemaError::join_nonempty(errs))
            }
        }
    }

    /// Check that `schema_nsdef` uses actions in a way consistent with the
    /// specified `action_behavior`. When the behavior specifies that actions
    /// should not be used in groups and should not have attributes, then this
    /// function will return `Err` if it sees any action groups or attributes
    /// declared in the schema.
    fn check_action_behavior<N>(
        schema_nsdef: &json_schema::NamespaceDefinition<N>,
        action_behavior: ActionBehavior,
    ) -> crate::validator::err::Result<()> {
        if schema_nsdef
            .entity_types
            .iter()
            // The `name` in an entity type declaration cannot be qualified
            // with a namespace (it always implicitly takes the schema
            // namespace), so we do this comparison directly.
            .any(|(name, _)| name.to_smolstr() == crate::ast::ACTION_ENTITY_TYPE)
        {
            return Err(ActionEntityTypeDeclaredError {}.into());
        }
        if action_behavior == ActionBehavior::ProhibitAttributes {
            let mut actions_with_attributes: Vec<String> = Vec::new();
            for (name, a) in &schema_nsdef.actions {
                if a.attributes.is_some() {
                    actions_with_attributes.push(name.to_string());
                }
            }
            if !actions_with_attributes.is_empty() {
                actions_with_attributes.sort(); // TODO(#833): sort required for deterministic error messages
                return Err(
                    UnsupportedFeatureError(UnsupportedFeature::ActionAttributes(
                        actions_with_attributes,
                    ))
                    .into(),
                );
            }
        }

        Ok(())
    }
}

/// Holds a map from (fully qualified) [`InternalName`]s of common type
/// definitions to their corresponding [`json_schema::Type`]. The common type
/// [`InternalName`]s (keys in the map) are fully qualified, but inside the
/// [`json_schema::Type`]s (values in the map), entity/common type references may or
/// may not be fully qualified yet, depending on `N`; see notes on
/// [`json_schema::Type`].
#[derive(Debug, Clone)]
pub struct CommonTypeDefs<N> {
    pub(super) defs: HashMap<InternalName, json_schema::Type<N>>,
}

impl CommonTypeDefs<ConditionalName> {
    /// Construct a [`CommonTypeDefs<ConditionalName>`] by converting the
    /// structures used by the schema format to those used internally by the
    /// validator.
    pub(crate) fn from_raw_common_types(
        schema_file_type_def: impl IntoIterator<Item = (CommonTypeId, json_schema::CommonType<RawName>)>,
        schema_namespace: Option<&InternalName>,
    ) -> crate::validator::err::Result<Self> {
        let mut defs = HashMap::new();
        for (id, schema_ty) in schema_file_type_def {
            let name = RawName::new_from_unreserved(id.into(), schema_ty.loc)
                .qualify_with(schema_namespace); // the declaration name is always (unconditionally) prefixed by the current/active namespace
            match defs.entry(name) {
                Entry::Vacant(ventry) => {
                    ventry.insert(
                        schema_ty
                            .ty
                            .conditionally_qualify_type_references(schema_namespace),
                    );
                }
                Entry::Occupied(oentry) => {
                    return Err(SchemaError::DuplicateCommonType(DuplicateCommonTypeError {
                        ty: oentry.key().clone(),
                    }));
                }
            }
        }
        Ok(Self { defs })
    }

    /// Construct a [`CommonTypeDefs<ConditionalName>`] by converting the
    /// structures used by the schema format to those used internally by the
    /// validator; but unlike `from_raw_common_types()`, this function allows you to
    /// directly supply [`ConditionalName`]s in the typedefs
    pub(crate) fn from_conditionalname_typedefs(
        input_type_defs: HashMap<UnreservedId, json_schema::Type<ConditionalName>>,
        schema_namespace: Option<&InternalName>,
    ) -> crate::validator::err::Result<Self> {
        let mut defs = HashMap::with_capacity(input_type_defs.len());
        for (id, schema_ty) in input_type_defs {
            let name = RawName::new_from_unreserved(id, None).qualify_with(schema_namespace); // the declaration name is always (unconditionally) prefixed by the current/active namespace
            match defs.entry(name) {
                Entry::Vacant(ventry) => {
                    ventry.insert(schema_ty);
                }
                Entry::Occupied(oentry) => {
                    return Err(SchemaError::DuplicateCommonType(DuplicateCommonTypeError {
                        ty: oentry.key().clone(),
                    }));
                }
            }
        }
        Ok(Self { defs })
    }

    /// Construct a [`CommonTypeDefs<ConditionalName>`] representing a single
    /// typedef in the given namespace.
    ///
    /// Unlike [`from_conditionalname_typedefs()`], this function cannot fail,
    /// because there is only one typedef so it cannot have a name collision
    /// with itself
    pub(crate) fn from_conditionalname_typedef(
        (id, schema_ty): (UnreservedId, json_schema::Type<ConditionalName>),
        schema_namespace: Option<&InternalName>,
    ) -> Self {
        Self {
            defs: HashMap::from_iter([(
                RawName::new_from_unreserved(id, None).qualify_with(schema_namespace),
                schema_ty,
            )]),
        }
    }

    /// Convert this [`CommonTypeDefs<ConditionalName>`] into a
    /// [`CommonTypeDefs<InternalName>`] by fully-qualifying all typenames that
    /// appear anywhere in any definitions.
    ///
    /// `all_defs` needs to contain the full set of all fully-qualified typenames
    /// and actions that are defined in the schema (in all schema fragments).
    pub fn fully_qualify_type_references(
        self,
        all_defs: &AllDefs,
    ) -> Result<CommonTypeDefs<InternalName>, TypeNotDefinedError> {
        Ok(CommonTypeDefs {
            defs: self
                .defs
                .into_iter()
                .map(|(k, v)| Ok((k, v.fully_qualify_type_references(all_defs)?)))
                .partition_nonempty()?,
        })
    }
}

/// Holds a map from (fully qualified) [`EntityType`]s (names of entity types) to
/// their corresponding [`EntityTypeFragment`]. The [`EntityType`] keys in
/// the map are fully qualified, but inside the [`EntityTypeFragment`]s (values
/// in the map), entity/common type references may or may not be fully qualified
/// yet, depending on `N`; see notes on [`EntityTypeFragment`].
///
/// Inside the [`EntityTypeFragment`]s, entity type parents and attributes may
/// reference undeclared entity/common types (that will be declared in a
/// different schema fragment).
///
/// All [`EntityType`] keys in this map are declared in this schema fragment.
#[derive(Debug, Clone)]
pub struct EntityTypesDef<N> {
    pub(super) defs: HashMap<EntityType, EntityTypeFragment<N>>,
}

impl<N> EntityTypesDef<N> {
    /// Construct an empty [`EntityTypesDef`] defining no entity types.
    pub fn new() -> Self {
        Self {
            defs: HashMap::new(),
        }
    }
}

impl EntityTypesDef<ConditionalName> {
    /// Construct a [`EntityTypesDef<ConditionalName>`] by converting the
    /// structures used by the schema format to those used internally by the
    /// validator.
    pub(crate) fn from_raw_entity_types(
        schema_files_types: impl IntoIterator<Item = (UnreservedId, json_schema::EntityType<RawName>)>,
        schema_namespace: Option<&InternalName>,
    ) -> crate::validator::err::Result<Self> {
        let mut defs: HashMap<EntityType, _> = HashMap::new();
        for (id, entity_type) in schema_files_types {
            let ety = internal_name_to_entity_type(
                RawName::new_from_unreserved(id, entity_type.loc.clone())
                    .qualify_with(schema_namespace), // the declaration name is always (unconditionally) prefixed by the current/active namespace
            )?;
            match defs.entry(ety) {
                Entry::Vacant(ventry) => {
                    ventry.insert(EntityTypeFragment::from_raw_entity_type(
                        entity_type,
                        schema_namespace,
                    ));
                }
                Entry::Occupied(entry) => {
                    return Err(DuplicateEntityTypeError {
                        ty: entry.key().clone(),
                    }
                    .into());
                }
            }
        }
        Ok(EntityTypesDef { defs })
    }

    /// Convert this [`EntityTypesDef<ConditionalName>`] into a
    /// [`EntityTypesDef<InternalName>`] by fully-qualifying all typenames that
    /// appear anywhere in any definitions.
    ///
    /// `all_defs` needs to contain the full set of all fully-qualified typenames
    /// and actions that are defined in the schema (in all schema fragments).
    pub fn fully_qualify_type_references(
        self,
        all_defs: &AllDefs,
    ) -> Result<EntityTypesDef<InternalName>, TypeNotDefinedError> {
        Ok(EntityTypesDef {
            defs: self
                .defs
                .into_iter()
                .map(|(k, v)| Ok((k, v.fully_qualify_type_references(all_defs)?)))
                .partition_nonempty()?,
        })
    }
}

/// Holds the attributes and parents information for an entity type definition.
///
/// In this representation, references to common types may not yet have been
/// fully resolved/inlined, and `parents`, `attributes`, and `tags` may all
/// reference undeclared entity/common types. Furthermore, entity/common type
/// references in `parents`, `attributes`, and `tags` may or may not be fully
/// qualified yet, depending on `N`.
#[derive(Debug, Clone)]
pub enum EntityTypeFragment<N> {
    Standard {
        /// Description of the attribute types for this entity type.
        ///
        /// This may contain references to common types which have not yet been
        /// resolved/inlined (e.g., because they are not defined in this schema
        /// fragment).
        /// In the extreme case, this may itself be just a common type pointing to a
        /// `Record` type defined in another fragment.
        attributes: json_schema::AttributesOrContext<N>,
        /// Direct parent entity types for this entity type.
        /// These entity types may be declared in a different namespace or schema
        /// fragment.
        ///
        /// We will check for undeclared parent types when combining fragments into
        /// a [`crate::validator::ValidatorSchema`].
        parents: HashSet<N>,
        /// Tag type for this entity type. `None` means no tags are allowed on this
        /// entity type.
        ///
        /// This may contain references to common types which have not yet been
        /// resolved/inlined (e.g., because they are not defined in this schema
        /// fragment).
        tags: Option<json_schema::Type<N>>,
    },
    Enum(NonEmpty<SmolStr>),
}

impl<N> EntityTypeFragment<N> {
    pub(crate) fn parents(&self) -> Box<dyn Iterator<Item = &N> + '_> {
        match self {
            Self::Standard { parents, .. } => Box::new(parents.iter()),
            Self::Enum(_) => Box::new(std::iter::empty()),
        }
    }
}

impl EntityTypeFragment<ConditionalName> {
    /// Construct a [`EntityTypeFragment<ConditionalName>`] by converting the
    /// structures used by the schema format to those used internally by the
    /// validator.
    pub(crate) fn from_raw_entity_type(
        schema_file_type: json_schema::EntityType<RawName>,
        schema_namespace: Option<&InternalName>,
    ) -> Self {
        match schema_file_type.kind {
            EntityTypeKind::Enum { choices } => Self::Enum(choices),
            EntityTypeKind::Standard(ty) => {
                Self::Standard {
                    attributes: ty
                        .shape
                        .conditionally_qualify_type_references(schema_namespace),
                    parents: ty
                        .member_of_types
                        .into_iter()
                        .map(|raw_name| {
                            // Only entity, not common, here for now; see #1064
                            raw_name
                                .conditionally_qualify_with(schema_namespace, ReferenceType::Entity)
                        })
                        .collect(),
                    tags: ty
                        .tags
                        .map(|tags| tags.conditionally_qualify_type_references(schema_namespace)),
                }
            }
        }
    }

    /// Convert this [`EntityTypeFragment<ConditionalName>`] into a
    /// [`EntityTypeFragment<InternalName>`] by fully-qualifying all typenames that
    /// appear anywhere in any definitions.
    ///
    /// `all_defs` needs to contain the full set of all fully-qualified typenames
    /// and actions that are defined in the schema (in all schema fragments).
    pub fn fully_qualify_type_references(
        self,
        all_defs: &AllDefs,
    ) -> Result<EntityTypeFragment<InternalName>, TypeNotDefinedError> {
        match self {
            Self::Enum(choices) => Ok(EntityTypeFragment::Enum(choices)),
            Self::Standard {
                attributes,
                parents,
                tags,
            } => {
                // Fully qualify typenames appearing in `attributes`
                let fully_qual_attributes = attributes.fully_qualify_type_references(all_defs);
                // Fully qualify typenames appearing in `parents`
                let parents: HashSet<InternalName> = parents
                    .into_iter()
                    .map(|parent| parent.resolve(all_defs))
                    .partition_nonempty()?;
                // Fully qualify typenames appearing in `tags`
                let fully_qual_tags = tags
                    .map(|tags| tags.fully_qualify_type_references(all_defs))
                    .transpose();
                // Now is the time to check whether any parents are dangling, i.e.,
                // refer to entity types that are not declared in any fragment (since we
                // now have the set of typenames that are declared in all fragments).
                let undeclared_parents: Option<NonEmpty<ConditionalName>> = NonEmpty::collect(
                    parents
                        .iter()
                        .filter(|ety| !all_defs.is_defined_as_entity(ety))
                        .map(|ety| {
                            ConditionalName::unconditional(ety.clone(), ReferenceType::Entity)
                        }),
                );
                match (fully_qual_attributes, fully_qual_tags, undeclared_parents) {
                    (Ok(attributes), Ok(tags), None) => Ok(EntityTypeFragment::Standard {
                        attributes,
                        parents,
                        tags,
                    }),
                    (Ok(_), Ok(_), Some(undeclared_parents)) => Err(TypeNotDefinedError {
                        undefined_types: undeclared_parents,
                    }),
                    (Err(e), Ok(_), None) | (Ok(_), Err(e), None) => Err(e),
                    (Err(e1), Err(e2), None) => {
                        Err(TypeNotDefinedError::join_nonempty(nonempty![e1, e2]))
                    }
                    (Err(e), Ok(_), Some(mut undeclared))
                    | (Ok(_), Err(e), Some(mut undeclared)) => {
                        undeclared.extend(e.undefined_types);
                        Err(TypeNotDefinedError {
                            undefined_types: undeclared,
                        })
                    }
                    (Err(e1), Err(e2), Some(mut undeclared)) => {
                        undeclared.extend(e1.undefined_types);
                        undeclared.extend(e2.undefined_types);
                        Err(TypeNotDefinedError {
                            undefined_types: undeclared,
                        })
                    }
                }
            }
        }
    }
}

/// Holds a map from (fully qualified) [`EntityUID`]s of action definitions
/// to their corresponding [`ActionFragment`]. The action [`EntityUID`]s (keys
/// in the map) are fully qualified, but inside the [`ActionFragment`]s (values
/// in the map), entity/common type references (including references to other actions)
/// may or may not be fully qualified yet, depending on `N` and `A`. See notes
/// on [`ActionFragment`].
///
/// The [`ActionFragment`]s may also reference undeclared entity/common types
/// and actions (that will be declared in a different schema fragment).
///
/// The current schema format specification does not include multiple action entity
/// types. All action entities are required to use a single `Action` entity
/// type. However, the action entity type may be namespaced, so an action entity
/// may have a fully qualified entity type `My::Namespace::Action`.
#[derive(Debug, Clone)]
pub struct ActionsDef<N, A> {
    pub(super) actions: HashMap<EntityUID, ActionFragment<N, A>>,
}

impl<N, A> ActionsDef<N, A> {
    /// Construct an empty [`ActionsDef`] defining no entity types.
    pub fn new() -> Self {
        Self {
            actions: HashMap::new(),
        }
    }
}

#[cfg_attr(not(feature = "extended-schema"), allow(unused_variables))]
fn create_action_entity_uid_default_type(
    action_name: &SmolStr,
    action_type: &json_schema::ActionType<RawName>,
    schema_namespace: Option<&InternalName>,
) -> json_schema::ActionEntityUID<InternalName> {
    let action_id_str = action_name.clone();
    #[cfg(feature = "extended-schema")]
    let action_id_loc = action_type.defn_loc.clone();
    #[cfg(feature = "extended-schema")]
    // the declaration name is always (unconditionally) prefixed by the current/active namespace
    return json_schema::ActionEntityUID::default_type_with_loc(action_id_str, action_id_loc)
        .qualify_with(schema_namespace);

    #[cfg(not(feature = "extended-schema"))]
    // the declaration name is always (unconditionally) prefixed by the current/active namespace
    json_schema::ActionEntityUID::default_type(action_id_str).qualify_with(schema_namespace)
}

impl ActionsDef<ConditionalName, ConditionalName> {
    /// Construct an [`ActionsDef<ConditionalName>`] by converting the structures used by the
    /// schema format to those used internally by the validator.
    pub(crate) fn from_raw_actions(
        schema_file_actions: impl IntoIterator<Item = (SmolStr, json_schema::ActionType<RawName>)>,
        schema_namespace: Option<&InternalName>,
        extensions: &Extensions<'_>,
    ) -> crate::validator::err::Result<Self> {
        let mut actions = HashMap::new();
        for (action_name, action_type) in schema_file_actions {
            let action_uid =
                create_action_entity_uid_default_type(&action_name, &action_type, schema_namespace);
            match actions.entry(action_uid.clone().try_into()?) {
                Entry::Vacant(ventry) => {
                    let frag = ActionFragment::from_raw_action(
                        ventry.key(),
                        action_type.clone(),
                        schema_namespace,
                        extensions,
                        action_type.loc.as_loc_ref(),
                    )?;
                    ventry.insert(frag);
                }
                Entry::Occupied(_) => {
                    return Err(DuplicateActionError(action_name).into());
                }
            }
        }
        Ok(Self { actions })
    }

    /// Convert this [`ActionsDef<ConditionalName>`] into a
    /// [`ActionsDef<InternalName>`] by fully-qualifying all typenames that
    /// appear anywhere in any definitions.
    ///
    /// `all_defs` needs to contain the full set of all fully-qualified typenames
    /// and actions that are defined in the schema (in all schema fragments).
    pub fn fully_qualify_type_references(
        self,
        all_defs: &AllDefs,
    ) -> Result<ActionsDef<InternalName, EntityType>, SchemaError> {
        Ok(ActionsDef {
            actions: self
                .actions
                .into_iter()
                .map(|(k, v)| v.fully_qualify_type_references(all_defs).map(|v| (k, v)))
                .partition_nonempty()?,
        })
    }
}

/// Holds the information about an action that comprises an action definition.
///
/// In this representation, references to common types may not yet have been
/// fully resolved/inlined, and entity/common type references (including
/// references to other actions) may not yet be fully qualified, depending on
/// `N` and `A`. This [`ActionFragment`] may also reference undeclared entity/common
/// types and actions (that will be declared in a different schema fragment).
///
/// `A` is used for typenames in `applies_to`, and `N` is used for all other
/// type references.
#[derive(Debug, Clone)]
pub struct ActionFragment<N, A> {
    /// The type of the context record for this action. This may contain
    /// references to common types which have not yet been resolved/inlined
    /// (e.g., because they are not defined in this schema fragment).
    pub(super) context: json_schema::Type<N>,
    /// The principals and resources that an action can be applied to.
    pub(super) applies_to: ValidatorApplySpec<A>,
    /// The direct parent action entities for this action.
    /// These may be actions declared in a different namespace or schema
    /// fragment, and thus not declared yet.
    /// We will check for undeclared parents when combining fragments into a
    /// [`crate::validator::ValidatorSchema`].
    pub(super) parents: HashSet<json_schema::ActionEntityUID<N>>,
    /// The types for the attributes defined for this actions entity.
    /// Here, common types have been fully resolved/inlined.
    pub(super) attribute_types: Attributes,
    /// The values for the attributes defined for this actions entity, stored
    /// separately so that we can later extract these values to construct the
    /// actual `Entity` objects defined by the schema.
    pub(super) attributes: BTreeMap<SmolStr, PartialValue>,
    /// Source location - if available
    pub(super) loc: MaybeLoc,
}

impl ActionFragment<ConditionalName, ConditionalName> {
    pub(crate) fn from_raw_action(
        action_uid: &EntityUID,
        action_type: json_schema::ActionType<RawName>,
        schema_namespace: Option<&InternalName>,
        extensions: &Extensions<'_>,
        loc: Option<&Loc>,
    ) -> crate::validator::err::Result<Self> {
        let (principal_types, resource_types, context) = action_type
            .applies_to
            .map(|applies_to| {
                (
                    applies_to.principal_types,
                    applies_to.resource_types,
                    applies_to.context,
                )
            })
            .unwrap_or_default();
        let (attribute_types, attributes) = Self::convert_attr_jsonval_map_to_attributes(
            action_type.attributes.unwrap_or_default(),
            action_uid,
            extensions,
        )?;
        Ok(Self {
            context: context
                .into_inner()
                .conditionally_qualify_type_references(schema_namespace),
            applies_to: ValidatorApplySpec::<ConditionalName>::new(
                principal_types
                    .into_iter()
                    .map(|pty| {
                        pty.conditionally_qualify_with(schema_namespace, ReferenceType::Entity)
                    })
                    .collect(),
                resource_types
                    .into_iter()
                    .map(|rty| {
                        rty.conditionally_qualify_with(schema_namespace, ReferenceType::Entity)
                    })
                    .collect(),
            ),
            parents: action_type
                .member_of
                .unwrap_or_default()
                .into_iter()
                .map(|parent| parent.conditionally_qualify_type_references(schema_namespace))
                .collect(),
            attribute_types,
            attributes,
            loc: loc.into_maybe_loc(),
        })
    }

    /// Convert this [`ActionFragment<ConditionalName>`] into an
    /// [`ActionFragment<InternalName>`] by fully-qualifying all typenames that
    /// appear anywhere in any definitions.
    ///
    /// `all_defs` needs to contain the full set of all fully-qualified typenames
    /// and actions that are defined in the schema (in all schema fragments).
    pub fn fully_qualify_type_references(
        self,
        all_defs: &AllDefs,
    ) -> Result<ActionFragment<InternalName, EntityType>, SchemaError> {
        Ok(ActionFragment {
            context: self.context.fully_qualify_type_references(all_defs)?,
            applies_to: self.applies_to.fully_qualify_type_references(all_defs)?,
            parents: self
                .parents
                .into_iter()
                .map(|parent| parent.fully_qualify_type_references(all_defs))
                .partition_nonempty()?,
            attribute_types: self.attribute_types,
            attributes: self.attributes,
            loc: self.loc,
        })
    }

    fn convert_attr_jsonval_map_to_attributes(
        m: HashMap<SmolStr, CedarValueJson>,
        action_id: &EntityUID,
        extensions: &Extensions<'_>,
    ) -> crate::validator::err::Result<(Attributes, BTreeMap<SmolStr, PartialValue>)> {
        let mut attr_types: HashMap<SmolStr, Type> = HashMap::with_capacity(m.len());
        let mut attr_values: BTreeMap<SmolStr, PartialValue> = BTreeMap::new();
        let evaluator = RestrictedEvaluator::new(extensions);

        for (k, v) in m {
            let t = Self::jsonval_to_type_helper(&v, action_id);
            match t {
                Ok(ty) => attr_types.insert(k.clone(), ty),
                Err(e) => return Err(e),
            };

            // As an artifact of the limited `CedarValueJson` variants accepted by
            // `Self::jsonval_to_type_helper`, we know that this function will
            // never error. Also note that this is only ever executed when
            // action attributes are enabled, but they cannot be enabled when
            // using Cedar through the public API. This is fortunate because
            // handling an error here would mean adding a new error variant to
            // `SchemaError` in the public API, but we didn't make that enum
            // `non_exhaustive`, so any new variants are a breaking change.
            // PANIC SAFETY: see above
            #[allow(clippy::expect_used)]
            let e = v.into_expr(|| JsonDeserializationErrorContext::EntityAttribute { uid: action_id.clone(), attr: k.clone() }).expect("`Self::jsonval_to_type_helper` will always return `Err` for a `CedarValueJson` that might make `into_expr` return `Err`");
            let pv = evaluator
                .partial_interpret(e.as_borrowed())
                .map_err(|err| {
                    ActionAttrEvalError(EntityAttrEvaluationError {
                        uid: action_id.clone(),
                        attr_or_tag: k.clone(),
                        was_attr: true,
                        err,
                    })
                })?;
            attr_values.insert(k, pv);
        }
        Ok((
            Attributes::with_required_attributes(attr_types),
            attr_values,
        ))
    }

    /// Helper to get types from `CedarValueJson`s. Currently doesn't support all
    /// `CedarValueJson` types. Note: If this function is extended to cover move
    /// `CedarValueJson`s, we must update `convert_attr_jsonval_map_to_attributes` to
    /// handle errors that may occur when parsing these values. This will require
    /// a breaking change in the `SchemaError` type in the public API.
    fn jsonval_to_type_helper(
        v: &CedarValueJson,
        action_id: &EntityUID,
    ) -> crate::validator::err::Result<Type> {
        match v {
            CedarValueJson::Bool(_) => Ok(Type::primitive_boolean()),
            CedarValueJson::Long(_) => Ok(Type::primitive_long()),
            CedarValueJson::String(_) => Ok(Type::primitive_string()),
            CedarValueJson::Record(r) => {
                let mut required_attrs: HashMap<SmolStr, Type> = HashMap::with_capacity(r.len());
                for (k, v_prime) in r {
                    let t = Self::jsonval_to_type_helper(v_prime, action_id);
                    match t {
                        Ok(ty) => required_attrs.insert(k.clone(), ty),
                        Err(e) => return Err(e),
                    };
                }
                Ok(Type::record_with_required_attributes(
                    required_attrs,
                    OpenTag::ClosedAttributes,
                ))
            }
            CedarValueJson::Set(v) => match v.first() {
                //sets with elements of different types will be rejected elsewhere
                None => Err(ActionAttributesContainEmptySetError {
                    uid: action_id.clone(),
                }
                .into()),
                Some(element) => {
                    let element_type = Self::jsonval_to_type_helper(element, action_id);
                    match element_type {
                        Ok(t) => Ok(Type::Set {
                            element_type: Some(Box::new(t)),
                        }),
                        Err(_) => element_type,
                    }
                }
            },
            CedarValueJson::EntityEscape { __entity: _ } => Err(UnsupportedActionAttributeError {
                uid: action_id.clone(),
                attr: "entity escape (`__entity`)".into(),
            }
            .into()),
            CedarValueJson::ExprEscape { __expr: _ } => Err(UnsupportedActionAttributeError {
                uid: action_id.clone(),
                attr: "expression escape (`__expr`)".into(),
            }
            .into()),
            CedarValueJson::ExtnEscape { __extn: _ } => Err(UnsupportedActionAttributeError {
                uid: action_id.clone(),
                attr: "extension function escape (`__extn`)".into(),
            }
            .into()),
            CedarValueJson::Null => Err(UnsupportedActionAttributeError {
                uid: action_id.clone(),
                attr: "null".into(),
            }
            .into()),
        }
    }
}

type ResolveFunc<T> =
    dyn FnOnce(&HashMap<&InternalName, ValidatorType>) -> crate::validator::err::Result<T>;
/// Represent a type that might be defined in terms of some common-type
/// definitions which are not necessarily available in the current namespace.
pub(crate) enum WithUnresolvedCommonTypeRefs<T> {
    WithUnresolved(Box<ResolveFunc<T>>, MaybeLoc),
    WithoutUnresolved(T, MaybeLoc),
}

impl<T: 'static> WithUnresolvedCommonTypeRefs<T> {
    pub fn new(
        f: impl FnOnce(&HashMap<&InternalName, ValidatorType>) -> crate::validator::err::Result<T>
            + 'static,
        loc: MaybeLoc,
    ) -> Self {
        Self::WithUnresolved(Box::new(f), loc)
    }

    pub fn loc(&self) -> Option<&Loc> {
        match self {
            WithUnresolvedCommonTypeRefs::WithUnresolved(_, loc) => loc.as_loc_ref(),
            WithUnresolvedCommonTypeRefs::WithoutUnresolved(_, loc) => loc.as_loc_ref(),
        }
    }

    pub fn map<U: 'static>(
        self,
        f: impl FnOnce(T) -> U + 'static,
    ) -> WithUnresolvedCommonTypeRefs<U> {
        match self {
            Self::WithUnresolved(_, ref loc) => {
                let loc = loc.clone();
                WithUnresolvedCommonTypeRefs::new(
                    |common_type_defs| self.resolve_common_type_refs(common_type_defs).map(f),
                    loc,
                )
            }
            Self::WithoutUnresolved(v, loc) => {
                WithUnresolvedCommonTypeRefs::WithoutUnresolved(f(v), loc)
            }
        }
    }

    /// Resolve references to common types by inlining their definitions from
    /// the given `HashMap`.
    ///
    /// Be warned that `common_type_defs` should contain all definitions, from
    /// all schema fragments.
    /// If `self` references any type not in `common_type_defs`, this will
    /// return a `TypeNotDefinedError`.
    pub fn resolve_common_type_refs(
        self,
        common_type_defs: &HashMap<&InternalName, ValidatorType>,
    ) -> crate::validator::err::Result<T> {
        match self {
            WithUnresolvedCommonTypeRefs::WithUnresolved(f, _loc) => f(common_type_defs),
            WithUnresolvedCommonTypeRefs::WithoutUnresolved(v, _loc) => Ok(v),
        }
    }
}

impl<T: 'static> From<T> for WithUnresolvedCommonTypeRefs<T> {
    fn from(value: T) -> Self {
        Self::WithoutUnresolved(value, None)
    }
}

impl From<Type> for WithUnresolvedCommonTypeRefs<ValidatorType> {
    fn from(value: Type) -> Self {
        Self::WithoutUnresolved(
            ValidatorType {
                ty: value,
                #[cfg(feature = "extended-schema")]
                loc: None,
            },
            None,
        )
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for WithUnresolvedCommonTypeRefs<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WithUnresolvedCommonTypeRefs::WithUnresolved(_, _) => {
                f.debug_tuple("WithUnresolved").finish()
            }
            WithUnresolvedCommonTypeRefs::WithoutUnresolved(v, _) => {
                f.debug_tuple("WithoutUnresolved").field(v).finish()
            }
        }
    }
}

impl TryInto<ValidatorNamespaceDef<ConditionalName, ConditionalName>>
    for json_schema::NamespaceDefinition<RawName>
{
    type Error = SchemaError;

    fn try_into(
        self,
    ) -> crate::validator::err::Result<ValidatorNamespaceDef<ConditionalName, ConditionalName>>
    {
        ValidatorNamespaceDef::from_namespace_definition(
            None,
            self,
            ActionBehavior::default(),
            Extensions::all_available(),
        )
    }
}

/// Convert a [`json_schema::Type`] (with fully-qualified names) into the
/// [`Type`] type used by the validator.
///
/// Conversion can fail if an entity or record attribute name is invalid. It
/// will also fail for some types that can be written in the schema, but are
/// not yet implemented in the typechecking logic.
pub(crate) fn try_jsonschema_type_into_validator_type(
    schema_ty: json_schema::Type<InternalName>,
    extensions: &Extensions<'_>,
    loc: MaybeLoc,
) -> crate::validator::err::Result<WithUnresolvedCommonTypeRefs<ValidatorType>> {
    match schema_ty {
        json_schema::Type::Type {
            ty: json_schema::TypeVariant::String,
            ..
        } => Ok(WithUnresolvedCommonTypeRefs::WithoutUnresolved(
            ValidatorType {
                ty: Type::primitive_string(),
                #[cfg(feature = "extended-schema")]
                loc: loc.clone(),
            },
            loc,
        )),
        json_schema::Type::Type {
            ty: json_schema::TypeVariant::Long,
            ..
        } => Ok(WithUnresolvedCommonTypeRefs::WithoutUnresolved(
            ValidatorType {
                ty: Type::primitive_long(),
                #[cfg(feature = "extended-schema")]
                loc: loc.clone(),
            },
            loc,
        )),
        json_schema::Type::Type {
            ty: json_schema::TypeVariant::Boolean,
            ..
        } => Ok(WithUnresolvedCommonTypeRefs::WithoutUnresolved(
            ValidatorType {
                ty: Type::primitive_boolean(),
                #[cfg(feature = "extended-schema")]
                loc: loc.clone(),
            },
            loc,
        )),
        json_schema::Type::Type {
            ty: json_schema::TypeVariant::Set { element },
            ..
        } => Ok(
            try_jsonschema_type_into_validator_type(*element, extensions, loc)?.map(|vt| {
                ValidatorType {
                    ty: Type::set(vt.ty),
                    #[cfg(feature = "extended-schema")]
                    loc: vt.loc,
                }
            }),
        ),
        json_schema::Type::Type {
            ty: json_schema::TypeVariant::Record(rty),
            ..
        } => try_record_type_into_validator_type(rty, extensions, loc),
        json_schema::Type::Type {
            ty: json_schema::TypeVariant::Entity { name },
            ..
        } => Ok(WithUnresolvedCommonTypeRefs::WithoutUnresolved(
            ValidatorType {
                ty: Type::named_entity_reference(internal_name_to_entity_type(name)?),
                #[cfg(feature = "extended-schema")]
                loc: loc.clone(),
            },
            loc,
        )),
        json_schema::Type::Type {
            ty: json_schema::TypeVariant::Extension { name },
            ..
        } => {
            let extension_type_name = Name::unqualified_name(name);
            if extensions.ext_types().contains(&extension_type_name) {
                Ok(Type::extension(extension_type_name).into())
            } else {
                let suggested_replacement = fuzzy_search(
                    &extension_type_name.to_string(),
                    &extensions
                        .ext_types()
                        .map(|n| n.to_string())
                        .collect::<Vec<_>>(),
                );
                Err(SchemaError::UnknownExtensionType(
                    UnknownExtensionTypeError {
                        actual: extension_type_name,
                        suggested_replacement,
                    },
                ))
            }
        }
        json_schema::Type::CommonTypeRef { type_name, .. } => {
            Ok(WithUnresolvedCommonTypeRefs::new(
                move |common_type_defs| {
                    common_type_defs
                        .get(&type_name)
                        .cloned()
                        // We should always have `Some` here, because if the common type
                        // wasn't defined, that error should have been caught earlier,
                        // when the `json_schema::Type<InternalName>` was created by
                        // resolving a `ConditionalName` into a fully-qualified
                        // `InternalName`.
                        // Nonetheless, instead of panicking if that internal
                        // invariant is violated, it's easy to return this dynamic
                        // error instead.
                        .ok_or_else(|| CommonTypeInvariantViolationError { name: type_name }.into())
                },
                loc,
            ))
        }
        json_schema::Type::Type {
            ty: json_schema::TypeVariant::EntityOrCommon { type_name },
            ..
        } => {
            let loc_clone = loc.clone();
            Ok(WithUnresolvedCommonTypeRefs::new(
                move |common_type_defs| {
                    #[cfg_attr(not(feature = "extended-schema"), allow(unused_variables))]
                    let loc: MaybeLoc = loc.clone();

                    // First check if it's a common type, because in the edge case where
                    // the name is both a valid common type name and a valid entity type
                    // name, we give preference to the common type (see RFC 24).
                    match common_type_defs.get(&type_name) {
                        Some(def) => Ok(def.clone()),
                        None => {
                            // It wasn't a common type, so we assume it must be a valid
                            // entity type. Otherwise, we would have had an error earlier,
                            // when the `json_schema::Type<InternalName>` was created by
                            // resolving a `ConditionalName` into a fully-qualified
                            // `InternalName`.
                            Ok(ValidatorType {
                                ty: Type::named_entity_reference(internal_name_to_entity_type(
                                    type_name,
                                )?),
                                #[cfg(feature = "extended-schema")]
                                loc,
                            })
                        }
                    }
                },
                loc_clone,
            ))
        }
    }
}

/// Convert a [`json_schema::RecordType`] (with fully qualified names) into the
/// [`Type`] type used by the validator.
#[cfg_attr(not(feature = "extended-schema"), allow(unused_variables))]
pub(crate) fn try_record_type_into_validator_type(
    rty: json_schema::RecordType<InternalName>,
    extensions: &Extensions<'_>,
    loc: MaybeLoc,
) -> crate::validator::err::Result<WithUnresolvedCommonTypeRefs<ValidatorType>> {
    if cfg!(not(feature = "partial-validate")) && rty.additional_attributes {
        Err(UnsupportedFeatureError(UnsupportedFeature::OpenRecordsAndEntities).into())
    } else {
        #[cfg(feature = "extended-schema")]
        let attr_loc = loc.clone();
        #[cfg(not(feature = "extended-schema"))]
        let attr_loc = None;
        Ok(
            parse_record_attributes(rty.attributes.into_iter(), extensions, attr_loc)?.map(
                move |attrs| ValidatorType {
                    ty: Type::record_with_attributes(
                        attrs,
                        if rty.additional_attributes {
                            OpenTag::OpenAttributes
                        } else {
                            OpenTag::ClosedAttributes
                        },
                    ),
                    #[cfg(feature = "extended-schema")]
                    loc,
                },
            ),
        )
    }
}

/// Given the attributes for an entity or record type in the schema file format
/// structures (but with fully-qualified names), convert the types of the
/// attributes into the [`Type`] data structure used by the validator, and
/// return the result as an [`Attributes`] structure.
#[cfg_attr(not(feature = "extended-schema"), allow(unused_variables))]
fn parse_record_attributes(
    attrs: impl IntoIterator<Item = (SmolStr, json_schema::TypeOfAttribute<InternalName>)>,
    extensions: &Extensions<'_>,
    loc: MaybeLoc,
) -> crate::validator::err::Result<WithUnresolvedCommonTypeRefs<Attributes>> {
    let attrs_with_common_type_refs = attrs
        .into_iter()
        .map(|(attr, ty)| -> crate::validator::err::Result<_> {
            #[cfg(feature = "extended-schema")]
            let loc = ty.loc;
            #[cfg(not(feature = "extended-schema"))]
            let loc = None;
            Ok((
                attr,
                (
                    try_jsonschema_type_into_validator_type(ty.ty.clone(), extensions, loc)?,
                    ty.required,
                ),
            ))
        })
        .collect::<crate::validator::err::Result<Vec<_>>>()?;

    Ok(WithUnresolvedCommonTypeRefs::new(
        |common_type_defs| {
            attrs_with_common_type_refs
                .into_iter()
                .map(|(s, (attr_ty, is_req))| {
                    let loc = attr_ty.loc().into_maybe_loc();
                    attr_ty
                        .resolve_common_type_refs(common_type_defs)
                        .map(|ty| {
                            #[cfg(feature = "extended-schema")]
                            let ty_pair = (s, AttributeType::new_with_loc(ty.ty, is_req, loc));
                            #[cfg(not(feature = "extended-schema"))]
                            let ty_pair = (s, AttributeType::new(ty.ty, is_req));
                            ty_pair
                        })
                })
                .collect::<crate::validator::err::Result<Vec<_>>>()
                .map(Attributes::with_attributes)
        },
        loc,
    ))
}

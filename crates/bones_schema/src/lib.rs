//! Simple reflection system based on the `#[repr(C)]` memory layout.

#![warn(missing_docs)]
// This cfg_attr is needed because `rustdoc::all` includes lints not supported on stable
#![cfg_attr(doc, allow(unknown_lints))]
#![deny(rustdoc::all)]
// This allows us to use our stable polyfills for nightly APIs under the same name.
#![allow(unstable_name_collisions)]

use std::{alloc::Layout, any::TypeId, borrow::Cow};

/// The prelude.
pub mod prelude {
    pub use crate::{
        alloc::SchemaVec, ptr::*, registry::*, FromType, HasSchema, Primitive, RawClone,
        RawDefault, RawDrop, Schema, SchemaData, SchemaKind, SchemaLayoutInfo, StructField,
        StructSchema, TypeDatas,
    };
    #[cfg(feature = "derive")]
    pub use bones_schema_macros::*;
    pub use ulid::Ulid;
}
use bones_utils::prelude::*;
use prelude::*;

#[cfg(feature = "derive")]
pub use bones_schema_macros::*;

pub mod alloc;
pub mod ptr;
pub mod registry;

mod std_impls;

#[cfg(feature = "serde")]
mod ser_de;

/// Trait implemented for types that have a [`Schema`].
///
/// # Safety
///
/// This trait is unsafe to implement manually because it makes claims about the memory layout of a
/// type that may be depended on in unsafe code, but it is safe to derive [`HasSchema`] on supported
/// types.
///
/// If implemented manually, you must ensure that the schema accurately describes the memory layout
/// of the type, or else accessing the type according to the schema would be unsound.
pub unsafe trait HasSchema: Sync + Send + 'static {
    /// Get this type's [`Schema`].
    fn schema() -> &'static Schema;

    /// Cast a reference of this type to a reference of another type with the same memory layout.
    ///
    /// # Panics
    ///
    /// Panics if the schema of `T` doesn't match the schema of `Self`.
    #[track_caller]
    fn cast<T: HasSchema>(&self) -> &T {
        self.try_cast().expect(SchemaMismatchError::MSG)
    }

    /// Cast a reference of this type to a reference of another type with the same memory layout.
    ///
    /// # Errors
    ///
    /// Errors if the schema of `T` doesn't match the schema of `Self`.
    fn try_cast<T: HasSchema>(&self) -> Result<&T, SchemaMismatchError> {
        let s1 = Self::schema();
        let s2 = T::schema();
        if s1.represents(s2) {
            // SAFE: the schemas have the same memory representation.
            unsafe { Ok(&*(self as *const Self as *const T)) }
        } else {
            Err(SchemaMismatchError)
        }
    }

    /// Cast a mutable reference of this type to a reference of another type with the same memory
    /// layout.
    ///
    /// # Panics
    ///
    /// Panics if the schema of `T` doesn't match the schema of `Self`.
    #[track_caller]
    fn cast_mut<T: HasSchema>(&mut self) -> &mut T {
        self.try_cast_mut().expect(SchemaMismatchError::MSG)
    }

    /// Cast a mutable reference of this type to a reference of another type with the same memory
    /// layout.
    ///
    /// # Errors
    ///
    /// Errors if the schema of `T` doesn't match the schema of `Self`.
    fn try_cast_mut<T: HasSchema>(&mut self) -> Result<&mut T, SchemaMismatchError> {
        let s1 = Self::schema();
        let s2 = T::schema();
        if s1.represents(s2) {
            // SAFE: the schemas have the same memory representation.
            unsafe { Ok(&mut *(self as *mut Self as *mut T)) }
        } else {
            Err(SchemaMismatchError)
        }
    }
}

/// A schema registered with the [`SCHEMA_REGISTRY`].
#[derive(Deref, Clone, Debug)]
pub struct Schema {
    id: SchemaId,
    #[deref]
    data: SchemaData,
    layout: Layout,
    field_offsets: &'static [(Option<&'static str>, usize)],
}

impl Schema {
    /// Get the registered, unique ID of the [`Schema`].
    #[inline]
    pub fn id(&self) -> SchemaId {
        self.id
    }

    /// Get a static reference to the [`Schema`] that was registered.
    #[inline]
    pub fn schema(&self) -> &SchemaData {
        &self.data
    }

    /// Get the [`Layout`] of the [`Schema`].
    #[inline]
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// If this schema represents a struct, this returns the list of fields, with the names of the
    /// fields, and their byte offsets from the beginning of the struct.
    #[inline]
    pub fn field_offsets(&self) -> &'static [(Option<&'static str>, usize)] {
        self.field_offsets
    }
}

impl SchemaId {
    /// Get the schema associated to the ID.
    pub fn get(&self) -> &'static Schema {
        SCHEMA_REGISTRY.get(*self)
    }
}

impl Schema {
    /// Returns whether or not this schema represents the same memory layout as the other schema,
    /// and you can safely cast a pointer to one to a pointer to the other.
    pub fn represents(&self, other: &Schema) -> bool {
        // If these have equal type/schema ids.
        self.equivalent(other)
            // If the schemas don't have any opaque fields, and are equal to each-other, then they
            // have the same representation.
            || (!self.has_opaque() && !other.has_opaque() && {
                // FIXME: do we need to compare clone_fn and drop_fn!?
                match (&self.kind, &other.kind) {
                    (SchemaKind::Struct(s1), SchemaKind::Struct(s2)) => {
                        s1.fields.len() == s2.fields.len() &&
                            s1.fields.iter().zip(s2.fields.iter())
                            .all(|(f1, f2)| f1.schema.represents(f2.schema))
                    },
                    (SchemaKind::Vec(v1), SchemaKind::Vec(v2)) => v1.represents(v2),
                    (SchemaKind::Primitive(p1), SchemaKind::Primitive(p2)) => p1 == p2,
                    _ => false
                }
            })
    }

    /// Returns whether or not this schema is the same schema as another.
    ///
    /// This check is made by checking the ID of both schemas to see if they have a matching Rust
    /// [`TypeId`], or if they have a matching [`SchemaId`].
    pub fn equivalent(&self, other: &Schema) -> bool {
        self.id() == other.id()
    }
}

/// A schema information describing the memory layout of a type.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct SchemaData {
    /// The kind of schema.
    pub kind: SchemaKind,
    #[serde(skip)]
    /// Arbitrary type data assocated to the schema.
    ///
    /// The [`Ulid`] key is arbitrary, allows different types to add different kinds of data to the
    /// schema.
    pub type_data: TypeDatas,

    // NOTE: The fields below could be implemented as type datas, and it would be nicely elegant to
    // do so, but for performance reasons, we put them right in the [`Schema`] struct because
    // they're use is so common. If profiling does not reveal any performance issues with using them
    // as type datas, we may want to remove these fields in favor of the type data.
    /// The Rust [`TypeId`] that this [`Schema`] was created from, if it was created from a Rust
    /// type.
    #[serde(skip)]
    pub type_id: Option<TypeId>,
    /// The function pointer that may be used to clone data with this schema.
    #[serde(skip)]
    pub clone_fn: Option<unsafe extern "C-unwind" fn(src: *const u8, dst: *mut u8)>,
    /// The function pointer that may be used to drop data with this schema.
    #[serde(skip)]
    pub drop_fn: Option<unsafe extern "C-unwind" fn(ptr: *mut u8)>,
    /// The function pointer that may be used to write a default value to a pointer.
    #[serde(skip)]
    pub default_fn: Option<unsafe extern "C-unwind" fn(ptr: *mut u8)>,
}

/// A schema describes the data layout of a type, to enable dynamic access to the type's data
/// through a pointer.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum SchemaKind {
    /// The type represents a struct.
    Struct(StructSchema),
    /// Type represents a Rust [`Vec`], where each item in the vec has the contained [`Schema`].
    ///
    /// Since the type is a Rust [`Vec`] interactions with it must happen through the Rust [`Vec`]
    /// methods.
    ///
    /// The scripting solution must facilitate a way for scripts to access data in the [`Vec`] if it
    /// is to be readable/modifyable from scripts.
    Vec(&'static Schema),
    /// The type represents a primitive value.
    Primitive(Primitive),
}

impl SchemaKind {
    /// Get the primitive, if this is a primitive.
    pub fn as_primitive(&self) -> Option<&Primitive> {
        if let Self::Primitive(p) = self {
            Some(p)
        } else {
            None
        }
    }
    /// Get the struct, if this is a struct.
    pub fn as_struct(&self) -> Option<&StructSchema> {
        if let Self::Struct(s) = self {
            Some(s)
        } else {
            None
        }
    }
    /// Get the schema of the items in the vector, if this is a vector.
    pub fn as_vec(&self) -> Option<&'static Schema> {
        if let Self::Vec(v) = self {
            Some(v)
        } else {
            None
        }
    }
}

/// Layout information about the schema.
#[derive(Debug, Clone)]
pub struct SchemaLayoutInfo<'a> {
    /// The layout of the type.
    pub layout: Layout,
    /// The field offsets if this is a struct schema.
    pub field_offsets: Vec<(Option<&'a str>, usize)>,
}

/// The schema for a struct.
#[derive(Debug, Clone)]
pub struct StructSchema {
    /// The fields in the struct, in the order they are defined.
    pub fields: Vec<StructField>,
}

/// A field in a [`StructSchema`].
#[derive(Debug, Clone)]
pub struct StructField {
    /// The name of the field. Will be [`None`] if this is a field of a tuple struct.
    pub name: Option<Cow<'static, str>>,
    /// The schema of the field.
    pub schema: &'static Schema,
}

/// The type of primitive. In the case of the number types, the size can be determined from the
/// [`Layout`] defined in the [`Schema`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[serde(rename_all = "snake_case")]
pub enum Primitive {
    /// A boolean.
    Bool,
    /// [`u8`]
    U8,
    /// [`u16`]
    U16,
    /// [`u32`]
    U32,
    /// [`u64`]
    U64,
    /// [`u128`]
    U128,
    /// [`i8`]
    I8,
    /// [`i16`]
    I16,
    /// [`i32`]
    I32,
    /// [`i64`]
    I64,
    /// [`i128`]
    I128,
    /// [`f32`]
    F32,
    /// [`f64`]
    F64,
    /// A Rust [`String`]. Must be manipulated with Rust string methods.
    String,
    /// Opaque data that cannot described by a schema.
    Opaque {
        /// The size of the data.
        size: usize,
        /// The alignment of the data.
        align: usize,
    },
}

/// Container for storing type datas.
#[derive(Clone, Debug, Default)]
pub struct TypeDatas(HashMap<SchemaId, SchemaBox>);
impl TypeDatas {
    /// Get a type data out of the store.
    #[track_caller]
    pub fn get<T: HasSchema>(&self) -> Option<&T> {
        let schema = T::schema();
        self.0.get(&schema.id()).map(|x| x.cast())
    }

    /// Insert a type data into the store
    pub fn insert<T: HasSchema>(&mut self, data: T) {
        self.0.insert(T::schema().id(), SchemaBox::new(data));
    }
}

/// Trait implemented for types that can produce an instance of themselves from a type.
pub trait FromType<T> {
    /// Return the data for the type.
    fn from_type() -> Self;
}

impl SchemaData {
    /// Calculate the layout of the type represented by the schema.
    ///
    /// Usually you don't need to call this and should use the static, cached layout and field
    /// offsets from [`Schema::layout()`] and [`Schema::field_offsets()`].
    pub fn compute_layout_info(&self) -> SchemaLayoutInfo<'_> {
        let mut layout: Option<Layout> = None;
        let mut field_offsets = Vec::new();
        let mut offset;

        let extend_layout = |layout: &mut Option<Layout>, l| {
            if let Some(layout) = layout {
                let (new_layout, offset) = layout.extend(l).unwrap();
                *layout = new_layout;
                offset
            } else {
                *layout = Some(l);
                0
            }
        };

        match &self.kind {
            SchemaKind::Struct(s) => {
                for field in &s.fields {
                    let field_layout_info = field.schema.compute_layout_info();
                    offset = extend_layout(&mut layout, field_layout_info.layout);
                    field_offsets.push((field.name.as_deref(), offset));
                }
            }
            SchemaKind::Vec(_) => {
                extend_layout(&mut layout, Layout::new::<Vec<u8>>());
            }
            SchemaKind::Primitive(p) => {
                extend_layout(
                    &mut layout,
                    match p {
                        Primitive::Bool => Layout::new::<bool>(),
                        Primitive::U8 => Layout::new::<u8>(),
                        Primitive::U16 => Layout::new::<u16>(),
                        Primitive::U32 => Layout::new::<u32>(),
                        Primitive::U64 => Layout::new::<u64>(),
                        Primitive::U128 => Layout::new::<u128>(),
                        Primitive::I8 => Layout::new::<i8>(),
                        Primitive::I16 => Layout::new::<i16>(),
                        Primitive::I32 => Layout::new::<i32>(),
                        Primitive::I64 => Layout::new::<i64>(),
                        Primitive::I128 => Layout::new::<i128>(),
                        Primitive::F32 => Layout::new::<f32>(),
                        Primitive::F64 => Layout::new::<f64>(),
                        Primitive::String => Layout::new::<String>(),
                        Primitive::Opaque { size, align } => {
                            Layout::from_size_align(*size, *align).unwrap()
                        }
                    },
                );
            }
        }

        SchemaLayoutInfo {
            layout: layout
                // Handle ZST
                .unwrap_or_else(|| Layout::from_size_align(0, 1).unwrap())
                .pad_to_align(),
            field_offsets,
        }
    }

    /// Recursively checks whether or not the schema contains any [`Opaque`][Primitive::Opaque] primitives.
    pub fn has_opaque(&self) -> bool {
        match &self.kind {
            SchemaKind::Struct(s) => s.fields.iter().any(|field| field.schema.has_opaque()),
            SchemaKind::Vec(v) => v.has_opaque(),
            SchemaKind::Primitive(p) => matches!(p, Primitive::Opaque { .. }),
        }
    }
}

impl From<SchemaKind> for SchemaData {
    fn from(kind: SchemaKind) -> Self {
        Self {
            type_id: None,
            clone_fn: None,
            drop_fn: None,
            default_fn: None,
            kind,
            type_data: Default::default(),
        }
    }
}

/// Trait implemented automatically for types that implement [`Clone`] and can be used to clone the
/// type through raw pointers.
pub trait RawClone {
    /// Write the default value of the type to the pointer.
    ///
    /// # Safety
    ///
    /// The `dst` pointer must be aligned, writable, and have the same layout that this function is
    /// assocated to, and the `src` pointer must be readable and point to a valid instance of the
    /// type that this function is associated with.
    unsafe extern "C-unwind" fn raw_clone(src: *const u8, dst: *mut u8);
}
impl<T: Clone> RawClone for T {
    unsafe extern "C-unwind" fn raw_clone(src: *const u8, dst: *mut u8) {
        let t = &*(src as *const T);
        let t = t.clone();
        (dst as *mut T).write(t)
    }
}
/// Trait implemented automatically for types that implement [`Drop`] and can be used to drop the
/// type through a raw pointer.
pub trait RawDrop {
    /// Write the default value of the type to the pointer.
    ///
    /// # Safety
    ///
    /// The pointer must be aligned, writable, and have the same layout that this function is
    /// assocated to.
    unsafe extern "C-unwind" fn raw_drop(ptr: *mut u8);
}
impl<T> RawDrop for T {
    unsafe extern "C-unwind" fn raw_drop(ptr: *mut u8) {
        if std::mem::needs_drop::<T>() {
            (ptr as *mut T).drop_in_place()
        }
    }
}

/// Trait implemented automatically for types that implement [`Default`] and can be used to write
/// the default value of the type to a pointer.
pub trait RawDefault {
    /// Write the default value of the type to the pointer.
    ///
    /// # Safety
    ///
    /// The pointer must be aligned, writable, and have the same layout that this function is
    /// assocated to.
    unsafe extern "C-unwind" fn raw_default(dst: *mut u8);
}
impl<T: Default> RawDefault for T {
    unsafe extern "C-unwind" fn raw_default(dst: *mut u8) {
        let d = T::default();
        (dst as *mut T).write(d)
    }
}
//! Procedural macros for `rkyv_dyn`.

#![deny(
    rustdoc::broken_intra_doc_links,
    missing_docs,
    rustdoc::missing_crate_level_docs
)]

extern crate proc_macro;

use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    parse::{Parse, ParseStream, Result},
    parse_macro_input,
    spanned::Spanned,
    Attribute, Error, Ident, ItemImpl, ItemTrait, LitStr, Path, Token,
    Visibility,
};

enum Input {
    Impl(ItemImpl),
    Trait(ItemTrait),
}

impl Parse for Input {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut attrs = Attribute::parse_outer(input)?;

        let ahead = input.fork();
        ahead.parse::<Visibility>()?;
        ahead.parse::<Option<Token![unsafe]>>()?;

        if ahead.peek(Token![trait]) {
            let mut item: ItemTrait = input.parse()?;
            attrs.extend(item.attrs);
            item.attrs = attrs;
            Ok(Input::Trait(item))
        } else if ahead.peek(Token![impl]) {
            let mut item: ItemImpl = input.parse()?;
            if item.trait_.is_none() {
                let impl_token = item.impl_token;
                let ty = item.self_ty;
                let span = quote!(#impl_token #ty);
                let msg = "expected impl Trait for Type";
                return Err(Error::new_spanned(span, msg));
            }
            attrs.extend(item.attrs);
            item.attrs = attrs;
            Ok(Input::Impl(item))
        } else {
            Err(input.error("expected trait or impl block"))
        }
    }
}

struct Args {
    serialize: Option<LitStr>,
    deserialize: Option<Option<LitStr>>,
}

impl Parse for Args {
    fn parse(input: ParseStream) -> Result<Self> {
        mod kw {
            syn::custom_keyword!(serialize);
            syn::custom_keyword!(deserialize);
        }

        let mut serialize = None;
        let mut deserialize = None;

        let mut needs_punct = false;
        while !input.is_empty() {
            if needs_punct {
                input.parse::<Token![,]>()?;
            }

            if input.peek(kw::serialize) {
                if serialize.is_some() {
                    return Err(input.error("duplicate serialize argument"));
                }

                input.parse::<kw::serialize>()?;
                input.parse::<Token![=]>()?;
                serialize = Some(input.parse::<LitStr>()?);
            } else if input.peek(kw::deserialize) {
                if deserialize.is_some() {
                    return Err(input.error("duplicate deserialize argument"));
                }

                input.parse::<kw::deserialize>()?;
                if input.peek(Token![=]) {
                    input.parse::<Token![=]>()?;
                    deserialize = Some(Some(input.parse::<LitStr>()?));
                } else {
                    deserialize = Some(None);
                }
            } else {
                return Err(input.error(
                    "expected serialize = \"...\" or deserialize = \"...\" \
                     parameters",
                ));
            }

            needs_punct = true;
        }

        Ok(Args {
            serialize,
            deserialize,
        })
    }
}

/// Creates archivable trait objects and registers implementations.
///
/// Prepend to trait definitions and implementations. For generic
/// implementations, you may need to manually register impls with the trait
/// object system. See `register_impl` for more information.
///
/// See `ArchiveDyn` for usage information and examples.
///
/// # Parameters
///
/// - `serialize = "..."`: Chooses the name of the serialize trait. By default,
///   it will be named "Serialize" + your trait name.
/// - `deserialize`, `deserialize = "..."`: Adds deserialization support to the
///   archived trait. Similarly to the `name` parameter, you can choose the name
///   of the deserialize trait and by default it will be named "Deserialize" +
///   your trait name.
#[proc_macro_attribute]
pub fn archive_dyn(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let input = parse_macro_input!(item as Input);
    let args = parse_macro_input!(attr as Args);

    match apply_archive_dyn(&input, &args) {
        Ok(result) => result.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn apply_archive_dyn(input: &Input, args: &Args) -> Result<TokenStream> {
    let input_impl = match input {
        Input::Impl(ref input) => {
            if !input.generics.params.is_empty() {
                Error::new(
                    input.generics.span(),
                    "#[archive_dyn] can only register non-generic impls; call \
                     register_impl! with the concrete types to register and \
                     manually implement DeserializeDyn for archived types if \
                     necessary",
                )
                .to_compile_error()
            } else if let Some((_, ref trait_, _)) = input.trait_ {
                register_impl(&input, &args, trait_)?
            } else {
                Error::new(
                    input.span(),
                    "#[archive_dyn] is only valid on trait implementations",
                )
                .to_compile_error()
            }
        }
        Input::Trait(input) => generate_traits(input, args)?,
    };

    Ok(input_impl)
}

fn register_impl(
    input: &ItemImpl,
    args: &Args,
    trait_: &Path,
) -> Result<TokenStream> {
    let ty = &input.self_ty;

    let mut ser_trait = trait_.clone();
    let last = ser_trait.segments.last_mut().unwrap();
    if let Some(ar_name) = &args.serialize {
        last.ident = Ident::new(&ar_name.value(), ar_name.span());
    } else {
        last.ident =
            Ident::new(&format!("Serialize{}", last.ident), trait_.span());
    };

    let (de_trait, de_impl) = if let Some(deserialize) = &args.deserialize {
        let mut de_trait = trait_.clone();
        let last = de_trait.segments.last_mut().unwrap();
        if let Some(ua_name) = deserialize {
            last.ident = Ident::new(&ua_name.value(), ua_name.span());
        } else {
            last.ident = Ident::new(
                &format!("Deserialize{}", last.ident),
                trait_.span(),
            );
        };

        (
            de_trait,
            quote! {
                impl DeserializeDyn<dyn #ser_trait> for Archived<#ty>
                where
                    Archived<#ty>:
                        for<'a> Deserialize<#ty, (dyn DynDeserializer + 'a)>,
                {
                    unsafe fn deserialize_dyn(
                        &self,
                        deserializer: &mut dyn DynDeserializer,
                        alloc: &mut dyn FnMut(Layout) -> *mut u8,
                    ) -> Result<*mut (), DynError> {
                        let result =
                            alloc(core::alloc::Layout::new::<#ty>())
                            .cast::<#ty>();
                        assert!(!result.is_null());
                        result.write(self.deserialize(deserializer)?);
                        Ok(result as *mut ())
                    }

                    fn deserialize_dyn_metadata(
                        &self,
                        deserializer: &mut dyn DynDeserializer,
                    ) -> Result<
                        <dyn #ser_trait as ptr_meta::Pointee>::Metadata,
                        DynError,
                    > {
                        let null_trait_object_ptr =
                            core::ptr::null::<#ty>() as *const dyn #ser_trait;
                        unsafe {
                            Ok(core::mem::transmute(
                                ptr_meta::metadata(null_trait_object_ptr)
                            ))
                        }
                    }
                }
            },
        )
    } else {
        (trait_.clone(), quote! {})
    };

    Ok(quote! {
        #input

        const _: () = {
            use core::alloc::{Layout, LayoutError};
            use rkyv::{
                Archived,
                Deserialize,
            };
            use rkyv_dyn::{
                DeserializeDyn,
                DynDeserializer,
                DynError,
            };

            rkyv_dyn::register_impl!(Archived<#ty> as dyn #de_trait);

            #de_impl
        };
    })
}

fn generate_traits(input: &ItemTrait, args: &Args) -> Result<TokenStream> {
    let vis = &input.vis;

    let generic_params = input.generics.params.iter().map(|p| quote! { #p });
    let generic_params = quote! { #(#generic_params),* };

    let generic_args = input.generics.type_params().map(|p| {
        let name = &p.ident;
        quote! { #name }
    });
    let generic_args = quote! { #(#generic_args),* };

    let name = &input.ident;
    let ser_trait = args
        .serialize
        .as_ref()
        .map(|ar_name| Ident::new(&ar_name.value(), ar_name.span()))
        .unwrap_or_else(|| {
            Ident::new(&format!("Serialize{}", name), name.span())
        });

    let type_name_wheres = input.generics.type_params().map(|p| {
        let name = &p.ident;
        quote! { #name: TypeName }
    });
    let type_name_wheres = quote! { #(#type_name_wheres,)* };

    let (de_trait, de_trait_def, de_trait_impl, pointee_input) = if let Some(
        deserialize,
    ) =
        &args.deserialize
    {
        let de_trait = if let Some(ua_name) = deserialize {
            Ident::new(&ua_name.value(), ua_name.span())
        } else {
            Ident::new(&format!("Deserialize{}", name), name.span())
        };

        (
            de_trait.clone(),
            quote! {
                #[ptr_meta::pointee]
                #vis trait #de_trait<#generic_params>:
                    #name<#generic_args>
                    + rkyv_dyn::DeserializeDyn<dyn #ser_trait<#generic_args>>
                {}
            },
            quote! {
                impl<__T, #generic_params> #de_trait<#generic_args> for __T
                where
                    __T:
                        #name<#generic_args>
                        + DeserializeDyn<dyn #ser_trait<#generic_args>>,
                {}

                impl<__D, #generic_params>
                    DeserializeUnsized<dyn #ser_trait<#generic_args>, __D>
                    for dyn #de_trait<#generic_args>
                where
                    __D: Fallible + ?Sized,
                {
                    unsafe fn deserialize_unsized(
                        &self,
                        mut deserializer: &mut __D,
                        mut alloc: impl FnMut(Layout) -> *mut u8,
                    ) -> Result<*mut (), __E> {
                        self.deserialize_dyn(
                            &mut deserializer,
                            &mut alloc,
                        ).map_err(|e| *e.downcast().unwrap())
                    }

                    fn deserialize_metadata(
                        &self,
                        mut deserializer: &mut __D,
                    ) -> Result<
                        <
                            dyn #ser_trait<#generic_args> as ptr_meta::Pointee
                        >::Metadata,
                        __E,
                    > {
                        self.deserialize_dyn_metadata(&mut deserializer)
                            .map_err(|e| *e.downcast().unwrap())
                    }
                }
            },
            quote! {},
        )
    } else {
        (
            name.clone(),
            quote! {},
            quote! {},
            quote! { #[ptr_meta::pointee] },
        )
    };

    let build_type_name = if !input.generics.params.is_empty() {
        let dyn_name = format!("dyn {}<", de_trait);
        let mut results = input.generics.type_params().map(|p| {
            let name = &p.ident;
            quote! { #name::build_type_name(&mut f) }
        });
        let first = results.next().unwrap();
        quote! {
            f(#dyn_name);
            #first;
            #(f(", "); #results;)*
            f(">");
        }
    } else {
        quote! { f(stringify!(dyn #de_trait)); }
    };

    #[cfg(feature = "bytecheck")]
    let validation_impl = quote! {
        use bytecheck::CheckBytes;
        use rkyv::validation::LayoutRaw;
        use rkyv_dyn::validation::{
            CHECK_BYTES_REGISTRY,
            CheckDynError,
            DynContext,
        };

        impl<#generic_params> LayoutRaw
            for (dyn #de_trait<#generic_args> + '_)
        {
            fn layout_raw(
                metadata: <Self as ptr_meta::Pointee>::Metadata,
            ) -> Result<Layout, LayoutError> {
                Ok(metadata.layout())
            }
        }

        impl<#generic_params> CheckBytes<dyn DynContext + '_>
            for (dyn #de_trait<#generic_args> + '_)
        {
            type Error = CheckDynError;

            #[inline]
            unsafe fn check_bytes<'a>(
                value: *const Self,
                context: &mut (dyn DynContext + '_),
            ) -> Result<&'a Self, Self::Error> {
                let vtable = core::mem::transmute(ptr_meta::metadata(value));
                if let Some(validation) = CHECK_BYTES_REGISTRY.get(vtable) {
                    (validation.check_bytes_dyn)(value.cast(), context)?;
                    Ok(&*value)
                } else {
                    Err(CheckDynError::InvalidMetadata(vtable as usize as u64))
                }
            }
        }

        impl<__C, #generic_params> CheckBytes<__C>
            for (dyn #de_trait<#generic_args> + '_)
        where
            __C: DynContext,
        {
            type Error = CheckDynError;

            #[inline]
            unsafe fn check_bytes<'a>(
                value: *const Self,
                context: &mut __C,
            ) -> Result<&'a Self, Self::Error> {
                Self::check_bytes(value, context as &mut dyn DynContext)
            }
        }
    };

    #[cfg(not(feature = "bytecheck"))]
    let validation_impl = quote! {};

    Ok(quote! {
        #pointee_input
        #input

        #[ptr_meta::pointee]
        #vis trait #ser_trait<#generic_params>:
            #name<#generic_args> + rkyv_dyn::SerializeDyn
        {}

        #de_trait_def

        const _: ()  = {
            use core::alloc::{Layout, LayoutError};
            use rkyv::{
                ser::{ScratchSpace, Serializer},
                Archive,
                Archived,
                ArchivedMetadata,
                ArchivePointee,
                ArchiveUnsized,
                DeserializeUnsized,
                Fallible,
                SerializeUnsized,
            };
            use rkyv_dyn::{
                ArchivedDynMetadata,
                DynDeserializer,
                RegisteredImpl,
                SerializeDyn,
                DeserializeDyn,
                DynSerializer,
            };
            use rkyv_typename::TypeName;

            impl<__T, #generic_params> #ser_trait<#generic_args> for __T
            where
                __T: Archive + SerializeDyn + #name<#generic_args>,
                __T::Archived: RegisteredImpl<dyn #de_trait<#generic_args>>
            {}

            #de_trait_impl

            impl<#generic_params> TypeName for dyn #de_trait<#generic_args> + '_
            where
                #type_name_wheres
            {
                fn build_type_name<F: FnMut(&str)>(mut f: F) {
                    #build_type_name
                }
            }

            impl<#generic_params> ArchiveUnsized
                for dyn #ser_trait<#generic_args>
            {
                type Archived = dyn #de_trait<#generic_args>;
                type MetadataResolver = ();

                unsafe fn resolve_metadata(
                    &self,
                    _: usize,
                    _: Self::MetadataResolver,
                    out: *mut ArchivedMetadata<Self>.
                ) {
                    ArchivedDynMetadata::emplace(self.archived_type_id(), out);
                }
            }

            impl<#generic_params> ArchivePointee
                for dyn #de_trait<#generic_args>
            {
                type ArchivedMetadata = ArchivedDynMetadata<Self>;

                fn pointer_metadata(
                    archived: &Self::ArchivedMetadata,
                ) -> <Self as ptr_meta::Pointee>::Metadata {
                    archived.pointer_metadata()
                }
            }

            impl<__S, #generic_params> SerializeUnsized<__S>
                for dyn #ser_trait<#generic_args>
            where
                __S: ScratchSpace + Serializer + ?Sized,
            {
                fn serialize_unsized(
                    &self,
                    mut serializer: &mut __S,
                ) -> Result<usize, __E> {
                    self.serialize_dyn(&mut serializer)
                        .map_err(|e| *e.downcast::<__E>().unwrap())
                }

                fn serialize_metadata(
                    &self,
                    _: &mut __S,
                ) -> Result<Self::MetadataResolver, __E> {
                    Ok(())
                }
            }

            #validation_impl
        };
    })
}

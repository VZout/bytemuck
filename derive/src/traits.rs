#![allow(unused_imports)]
use std::{cmp, convert::TryFrom};

use proc_macro2::{Ident, Span, TokenStream, TokenTree};
use quote::{quote, quote_spanned, ToTokens};
use syn::{
  parse::{Parse, ParseStream, Parser},
  punctuated::Punctuated,
  spanned::Spanned,
  Result, *,
};

macro_rules! bail {
  ($msg:expr $(,)?) => {
    return Err(Error::new(Span::call_site(), &$msg[..]))
  };

  ( $msg:expr => $span_to_blame:expr $(,)? ) => {
    return Err(Error::new_spanned(&$span_to_blame, $msg))
  };
}

pub trait Derivable {
  fn ident(input: &DeriveInput) -> Result<syn::Path>;
  fn implies_trait() -> Option<TokenStream> {
    None
  }
  fn asserts(_input: &DeriveInput) -> Result<TokenStream> {
    Ok(quote!())
  }
  fn check_attributes(_ty: &Data, _attributes: &[Attribute]) -> Result<()> {
    Ok(())
  }
  fn trait_impl(_input: &DeriveInput) -> Result<(TokenStream, TokenStream)> {
    Ok((quote!(), quote!()))
  }
  fn requires_where_clause() -> bool {
    true
  }
}

pub struct Pod;

impl Derivable for Pod {
  fn ident(_: &DeriveInput) -> Result<syn::Path> {
    Ok(syn::parse_quote!(::bytemuck::Pod))
  }

  fn asserts(input: &DeriveInput) -> Result<TokenStream> {
    let repr = get_repr(&input.attrs)?;

    let completly_packed =
      repr.packed == Some(1) || repr.repr == Repr::Transparent;

    if !completly_packed && !input.generics.params.is_empty() {
      bail!("\
        Pod requires cannot be derived for non-packed types containing \
        generic parameters because the padding requirements can't be verified \
        for generic non-packed structs\
      " => input.generics.params.first().unwrap());
    }

    match &input.data {
      Data::Struct(_) => {
        let assert_no_padding = if !completly_packed {
          Some(generate_assert_no_padding(input)?)
        } else {
          None
        };
        let assert_fields_are_pod =
          generate_fields_are_trait(input, Self::ident(input)?)?;

        Ok(quote!(
          #assert_no_padding
          #assert_fields_are_pod
        ))
      }
      Data::Enum(_) => bail!("Deriving Pod is not supported for enums"),
      Data::Union(_) => bail!("Deriving Pod is not supported for unions"),
    }
  }

  fn check_attributes(_ty: &Data, attributes: &[Attribute]) -> Result<()> {
    let repr = get_repr(attributes)?;
    match repr.repr {
      Repr::C => Ok(()),
      Repr::Transparent => Ok(()),
      _ => {
        bail!("Pod requires the type to be #[repr(C)] or #[repr(transparent)]")
      }
    }
  }
}

pub struct AnyBitPattern;

impl Derivable for AnyBitPattern {
  fn ident(_: &DeriveInput) -> Result<syn::Path> {
    Ok(syn::parse_quote!(::bytemuck::AnyBitPattern))
  }

  fn implies_trait() -> Option<TokenStream> {
    Some(quote!(::bytemuck::Zeroable))
  }

  fn asserts(input: &DeriveInput) -> Result<TokenStream> {
    match &input.data {
      Data::Union(_) => Ok(quote!()), // unions are always `AnyBitPattern`
      Data::Struct(_) => generate_fields_are_trait(input, Self::ident(input)?),
      Data::Enum(_) => {
        bail!("Deriving AnyBitPattern is not supported for enums")
      }
    }
  }
}

pub struct Zeroable;

impl Derivable for Zeroable {
  fn ident(_: &DeriveInput) -> Result<syn::Path> {
    Ok(syn::parse_quote!(::bytemuck::Zeroable))
  }

  fn asserts(input: &DeriveInput) -> Result<TokenStream> {
    match &input.data {
      Data::Union(_) => Ok(quote!()), // unions are always `Zeroable`
      Data::Struct(_) => generate_fields_are_trait(input, Self::ident(input)?),
      Data::Enum(_) => bail!("Deriving Zeroable is not supported for enums"),
    }
  }
}

pub struct NoUninit;

impl Derivable for NoUninit {
  fn ident(_: &DeriveInput) -> Result<syn::Path> {
    Ok(syn::parse_quote!(::bytemuck::NoUninit))
  }

  fn check_attributes(ty: &Data, attributes: &[Attribute]) -> Result<()> {
    let repr = get_repr(attributes)?;
    match ty {
      Data::Struct(_) => match repr.repr {
        Repr::C | Repr::Transparent => Ok(()),
        _ => bail!("NoUninit requires the struct to be #[repr(C)] or #[repr(transparent)]"),
      },
      Data::Enum(_) => if repr.repr.is_integer() {
        Ok(())
      } else {
        bail!("NoUninit requires the enum to be an explicit #[repr(Int)]")
      },
      Data::Union(_) => bail!("NoUninit can only be derived on enums and structs")
    }
  }

  fn asserts(input: &DeriveInput) -> Result<TokenStream> {
    if !input.generics.params.is_empty() {
      bail!("NoUninit cannot be derived for structs containing generic parameters because the padding requirements can't be verified for generic structs");
    }

    match &input.data {
      Data::Struct(DataStruct { .. }) => {
        let assert_no_padding = generate_assert_no_padding(&input)?;
        let assert_fields_are_no_padding =
          generate_fields_are_trait(&input, Self::ident(input)?)?;

        Ok(quote!(
            #assert_no_padding
            #assert_fields_are_no_padding
        ))
      }
      Data::Enum(DataEnum { variants, .. }) => {
        if variants.iter().any(|variant| !variant.fields.is_empty()) {
          bail!("Only fieldless enums are supported for NoUninit")
        } else {
          Ok(quote!())
        }
      }
      Data::Union(_) => bail!("NoUninit cannot be derived for unions"), /* shouldn't be possible since we already error in attribute check for this case */
    }
  }

  fn trait_impl(_input: &DeriveInput) -> Result<(TokenStream, TokenStream)> {
    Ok((quote!(), quote!()))
  }
}

pub struct CheckedBitPattern;

impl Derivable for CheckedBitPattern {
  fn ident(_: &DeriveInput) -> Result<syn::Path> {
    Ok(syn::parse_quote!(::bytemuck::CheckedBitPattern))
  }

  fn check_attributes(ty: &Data, attributes: &[Attribute]) -> Result<()> {
    let repr = get_repr(attributes)?;
    match ty {
      Data::Struct(_) => match repr.repr {
        Repr::C | Repr::Transparent => Ok(()),
        _ => bail!("CheckedBitPattern derive requires the struct to be #[repr(C)] or #[repr(transparent)]"),
      },
      Data::Enum(DataEnum { variants,.. }) => {
        if !enum_has_fields(variants.iter()){
          if repr.repr.is_integer() {
            Ok(())
          } else {
            bail!("CheckedBitPattern requires the enum to be an explicit #[repr(Int)]")
          }
        } else if matches!(repr.repr, Repr::Rust) {
          bail!("the default Rust repr doesn't have a specified type layout")
        } else {
          Ok(())
        }
      }
      Data::Union(_) => bail!("CheckedBitPattern can only be derived on enums and structs")
    }
  }

  fn asserts(input: &DeriveInput) -> Result<TokenStream> {
    if !input.generics.params.is_empty() {
      bail!("CheckedBitPattern cannot be derived for structs containing generic parameters");
    }

    match &input.data {
      Data::Struct(DataStruct { .. }) => {
        let assert_fields_are_maybe_pod =
          generate_fields_are_trait(&input, Self::ident(input)?)?;

        Ok(assert_fields_are_maybe_pod)
      }
      Data::Enum(_) => Ok(quote!()), /* nothing needed, already guaranteed
      * OK by NoUninit */
      Data::Union(_) => bail!("Internal error in CheckedBitPattern derive"), /* shouldn't be possible since we already error in attribute check for this case */
    }
  }

  fn trait_impl(input: &DeriveInput) -> Result<(TokenStream, TokenStream)> {
    match &input.data {
      Data::Struct(DataStruct { fields, .. }) => {
        generate_checked_bit_pattern_struct(&input.ident, fields, &input.attrs)
      }
      Data::Enum(DataEnum { variants, .. }) => {
        generate_checked_bit_pattern_enum(input, variants)
      }
      Data::Union(_) => bail!("Internal error in CheckedBitPattern derive"), /* shouldn't be possible since we already error in attribute check for this case */
    }
  }
}

pub struct TransparentWrapper;

impl TransparentWrapper {
  fn get_wrapper_type(
    attributes: &[Attribute], fields: &Fields,
  ) -> Option<TokenStream> {
    let transparent_param = get_simple_attr(attributes, "transparent");
    transparent_param.map(|ident| ident.to_token_stream()).or_else(|| {
      let mut types = get_field_types(&fields);
      let first_type = types.next();
      if let Some(_) = types.next() {
        // can't guess param type if there is more than one field
        return None;
      } else {
        first_type.map(|ty| ty.to_token_stream())
      }
    })
  }
}

impl Derivable for TransparentWrapper {
  fn ident(input: &DeriveInput) -> Result<syn::Path> {
    let fields = get_struct_fields(input)?;

    let ty = match Self::get_wrapper_type(&input.attrs, &fields) {
      Some(ty) => ty,
      None => bail!(
        "\
        when deriving TransparentWrapper for a struct with more than one field \
        you need to specify the transparent field using #[transparent(T)]\
      "
      ),
    };

    Ok(syn::parse_quote!(::bytemuck::TransparentWrapper<#ty>))
  }

  fn asserts(input: &DeriveInput) -> Result<TokenStream> {
    let (impl_generics, _ty_generics, where_clause) =
      input.generics.split_for_impl();
    let fields = get_struct_fields(input)?;
    let wrapped_type = match Self::get_wrapper_type(&input.attrs, &fields) {
      Some(wrapped_type) => wrapped_type.to_string(),
      None => unreachable!(), /* other code will already reject this derive */
    };
    let mut wrapped_field_ty = None;
    let mut nonwrapped_field_tys = vec![];
    for field in fields.iter() {
      let field_ty = &field.ty;
      if field_ty.to_token_stream().to_string() == wrapped_type {
        if wrapped_field_ty.is_some() {
          bail!(
            "TransparentWrapper can only have one field of the wrapped type"
          );
        }
        wrapped_field_ty = Some(field_ty);
      } else {
        nonwrapped_field_tys.push(field_ty);
      }
    }
    if let Some(wrapped_field_ty) = wrapped_field_ty {
      Ok(quote!(
        const _: () = {
          #[repr(transparent)]
          struct AssertWrappedIsWrapped #impl_generics((u8, ::core::marker::PhantomData<#wrapped_field_ty>), #(#nonwrapped_field_tys),*) #where_clause;
          fn assert_zeroable<Z: ::bytemuck::Zeroable>() {}
          fn check #impl_generics () #where_clause {
            #(
              assert_zeroable::<#nonwrapped_field_tys>();
            )*
          }
        };
      ))
    } else {
      bail!("TransparentWrapper must have one field of the wrapped type")
    }
  }

  fn check_attributes(_ty: &Data, attributes: &[Attribute]) -> Result<()> {
    let repr = get_repr(attributes)?;

    match repr.repr {
      Repr::Transparent => Ok(()),
      _ => {
        bail!(
          "TransparentWrapper requires the struct to be #[repr(transparent)]"
        )
      }
    }
  }

  fn requires_where_clause() -> bool {
    false
  }
}

pub struct Contiguous;

impl Derivable for Contiguous {
  fn ident(_: &DeriveInput) -> Result<syn::Path> {
    Ok(syn::parse_quote!(::bytemuck::Contiguous))
  }

  fn trait_impl(input: &DeriveInput) -> Result<(TokenStream, TokenStream)> {
    let repr = get_repr(&input.attrs)?;

    let integer_ty = if let Some(integer_ty) = repr.repr.as_integer() {
      integer_ty
    } else {
      bail!("Contiguous requires the enum to be #[repr(Int)]");
    };

    let variants = get_enum_variants(input)?;
    if enum_has_fields(variants.clone()) {
      return Err(Error::new_spanned(
        &input,
        "Only fieldless enums are supported",
      ));
    }

    let mut variants_with_discriminator =
      VariantDiscriminantIterator::new(variants);

    let (min, max, count) = variants_with_discriminator.try_fold(
      (i64::max_value(), i64::min_value(), 0),
      |(min, max, count), res| {
        let discriminator = res?;
        Ok::<_, Error>((
          i64::min(min, discriminator),
          i64::max(max, discriminator),
          count + 1,
        ))
      },
    )?;

    if max - min != count - 1 {
      bail! {
        "Contiguous requires the enum discriminants to be contiguous",
      }
    }

    let min_lit = LitInt::new(&format!("{}", min), input.span());
    let max_lit = LitInt::new(&format!("{}", max), input.span());

    Ok((
      quote!(),
      quote! {
          type Int = #integer_ty;
          const MIN_VALUE: #integer_ty = #min_lit;
          const MAX_VALUE: #integer_ty = #max_lit;
      },
    ))
  }
}

fn get_struct_fields(input: &DeriveInput) -> Result<&Fields> {
  if let Data::Struct(DataStruct { fields, .. }) = &input.data {
    Ok(fields)
  } else {
    bail!("deriving this trait is only supported for structs")
  }
}

fn get_fields(input: &DeriveInput) -> Result<Fields> {
  match &input.data {
    Data::Struct(DataStruct { fields, .. }) => Ok(fields.clone()),
    Data::Union(DataUnion { fields, .. }) => Ok(Fields::Named(fields.clone())),
    Data::Enum(_) => bail!("deriving this trait is not supported for enums"),
  }
}

fn get_enum_variants<'a>(
  input: &'a DeriveInput,
) -> Result<impl Iterator<Item = &'a Variant> + Clone + 'a> {
  if let Data::Enum(DataEnum { variants, .. }) = &input.data {
    Ok(variants.iter())
  } else {
    bail!("deriving this trait is only supported for enums")
  }
}

fn get_field_types<'a>(
  fields: &'a Fields,
) -> impl Iterator<Item = &'a Type> + 'a {
  fields.iter().map(|field| &field.ty)
}

fn generate_checked_bit_pattern_struct(
  input_ident: &Ident, fields: &Fields, attrs: &[Attribute],
) -> Result<(TokenStream, TokenStream)> {
  let bits_ty = Ident::new(&format!("{}Bits", input_ident), input_ident.span());

  let repr = get_repr(attrs)?;

  let field_names = fields
    .iter()
    .enumerate()
    .map(|(i, field)| {
      field.ident.clone().unwrap_or_else(|| {
        Ident::new(&format!("field{}", i), input_ident.span())
      })
    })
    .collect::<Vec<_>>();
  let field_tys = fields.iter().map(|field| &field.ty).collect::<Vec<_>>();

  let field_name = &field_names[..];
  let field_ty = &field_tys[..];

  let derive_dbg =
    quote!(#[cfg_attr(not(target_arch = "spirv"), derive(Debug))]);

  Ok((
    quote! {
        #repr
        #[derive(Clone, Copy, ::bytemuck::AnyBitPattern)]
        #derive_dbg
        pub struct #bits_ty {
            #(#field_name: <#field_ty as ::bytemuck::CheckedBitPattern>::Bits,)*
        }
    },
    quote! {
        type Bits = #bits_ty;

        #[inline]
        #[allow(clippy::double_comparisons)]
        fn is_valid_bit_pattern(bits: &#bits_ty) -> bool {
            #(<#field_ty as ::bytemuck::CheckedBitPattern>::is_valid_bit_pattern(&{ bits.#field_name }) && )* true
        }
    },
  ))
}

fn generate_checked_bit_pattern_enum(
  input: &DeriveInput, variants: &Punctuated<Variant, Token![,]>,
) -> Result<(TokenStream, TokenStream)> {
  if enum_has_fields(variants.iter()) {
    generate_checked_bit_pattern_enum_with_fields(input, variants)
  } else {
    generate_checked_bit_pattern_enum_without_fields(input, variants)
  }
}

fn generate_checked_bit_pattern_enum_without_fields(
  input: &DeriveInput, variants: &Punctuated<Variant, Token![,]>,
) -> Result<(TokenStream, TokenStream)> {
  let span = input.span();
  let mut variants_with_discriminant =
    VariantDiscriminantIterator::new(variants.iter());

  let (min, max, count) = variants_with_discriminant.try_fold(
    (i64::max_value(), i64::min_value(), 0),
    |(min, max, count), res| {
      let discriminant = res?;
      Ok::<_, Error>((
        i64::min(min, discriminant),
        i64::max(max, discriminant),
        count + 1,
      ))
    },
  )?;

  let check = if count == 0 {
    quote_spanned!(span => false)
  } else if max - min == count - 1 {
    // contiguous range
    let min_lit = LitInt::new(&format!("{}", min), span);
    let max_lit = LitInt::new(&format!("{}", max), span);

    quote!(*bits >= #min_lit && *bits <= #max_lit)
  } else {
    // not contiguous range, check for each
    let variant_lits = VariantDiscriminantIterator::new(variants.iter())
      .map(|res| {
        let variant = res?;
        Ok(LitInt::new(&format!("{}", variant), span))
      })
      .collect::<Result<Vec<_>>>()?;

    // count is at least 1
    let first = &variant_lits[0];
    let rest = &variant_lits[1..];

    quote!(matches!(*bits, #first #(| #rest )*))
  };

  let repr = get_repr(&input.attrs)?;
  let integer = repr.repr.as_integer().unwrap(); // should be checked in attr check already
  Ok((
    quote!(),
    quote! {
        type Bits = #integer;

        #[inline]
        #[allow(clippy::double_comparisons)]
        fn is_valid_bit_pattern(bits: &Self::Bits) -> bool {
            #check
        }
    },
  ))
}

fn generate_checked_bit_pattern_enum_with_fields(
  input: &DeriveInput, variants: &Punctuated<Variant, Token![,]>,
) -> Result<(TokenStream, TokenStream)> {
  let representation = get_repr(&input.attrs)?;
  let vis = &input.vis;

  let derive_dbg =
    quote!(#[cfg_attr(not(target_arch = "spirv"), derive(Debug))]);

  match representation.repr {
    Repr::Rust => unreachable!(),
    repr @ (Repr::C | Repr::CWithDiscriminant(_)) => {
      let integer = match repr {
        Repr::C => quote!(::core::ffi::c_int),
        Repr::CWithDiscriminant(integer) => quote!(#integer),
        _ => unreachable!(),
      };

      let bits_repr = Representation { repr: Repr::C, ..representation };

      let bits_ty = Ident::new(&format!("{}Bits", input.ident), input.span());
      let fields_ty =
        Ident::new(&format!("{}Fields", input.ident), input.span());

      let variant_struct = variants
        .iter()
        .map(|v| Ident::new(&format!("{fields_ty}{}", v.ident), v.span()));

      let variant_struct_definitions =
        variant_struct.clone().zip(variants.iter()).map(|(variant_ty, v)| {
          let fields = v.fields.iter().map(|v| &v.ty);

          quote! {
            #[derive(::core::clone::Clone, ::core::marker::Copy, ::bytemuck::CheckedBitPattern)]
            #[repr(C)]
            #vis struct #variant_ty(#(#fields),*);
          }
        });

      let union_fields =
        variant_struct.clone().zip(variants.iter()).map(|(variant_ty, v)| {
          let variant_ty =
            Ident::new(&format!("{variant_ty}Bits"), input.span());
          let field_ident = &v.ident;
          quote! {
            #field_ident: #variant_ty
          }
        });

      let variant_checks = variant_struct
        .clone()
        .zip(VariantDiscriminantIterator::new(variants.iter()))
        .zip(variants.iter())
        .map(|((variant_ty, discriminant), v)| -> Result<_> {
          let discriminant = discriminant?;
          let discriminant = LitInt::new(&discriminant.to_string(), v.span());
          let ident = &v.ident;
          Ok(quote! {
            #discriminant => {
              let payload = unsafe { &bits.payload.#ident };
              <#variant_ty as ::bytemuck::CheckedBitPattern>::is_valid_bit_pattern(payload)
            }
          })
        })
        .collect::<Result<Vec<_>>>()?;

      Ok((
        quote! {
          #[derive(::core::clone::Clone, ::core::marker::Copy, ::bytemuck::AnyBitPattern)]
          #derive_dbg
          #bits_repr
          #vis struct #bits_ty {
            tag: #integer,
            payload: #fields_ty,
          }

          #[derive(::core::clone::Clone, ::core::marker::Copy, ::bytemuck::AnyBitPattern)]
          #[repr(C)]
          #[allow(non_snake_case)]
          #vis union #fields_ty {
            #(#union_fields,)*
          }

          #[cfg(not(target_arch = "spirv"))]
          impl ::core::fmt::Debug for #fields_ty {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
              let mut debug_struct = ::core::fmt::Formatter::debug_struct(f, ::core::stringify!(#fields_ty));
              ::core::fmt::DebugStruct::finish_non_exhaustive(&mut debug_struct)
            }
          }

          #(#variant_struct_definitions)*
        },
        quote! {
          type Bits = #bits_ty;

          #[inline]
          #[allow(clippy::double_comparisons)]
          fn is_valid_bit_pattern(bits: &Self::Bits) -> bool {
            match bits.tag {
              #(#variant_checks)*
              _ => false,
            }
          }
        },
      ))
    }
    Repr::Transparent => {
      if variants.len() != 1 {
        bail!("enums with more than one variant cannot be transparent")
      }

      let variant = &variants[0];

      let bits_ty = Ident::new(&format!("{}Bits", input.ident), input.span());
      let fields = variant.fields.iter().map(|v| &v.ty);

      Ok((
        quote! {
          #[derive(::core::clone::Clone, ::core::marker::Copy, ::bytemuck::CheckedBitPattern)]
          #[repr(C)]
          #vis struct #bits_ty(#(#fields),*);
        },
        quote! {
          type Bits = <#bits_ty as ::bytemuck::CheckedBitPattern>::Bits;

          #[inline]
          #[allow(clippy::double_comparisons)]
          fn is_valid_bit_pattern(bits: &Self::Bits) -> bool {
            <#bits_ty as ::bytemuck::CheckedBitPattern>::is_valid_bit_pattern(bits)
          }
        },
      ))
    }
    Repr::Integer(integer) => {
      let bits_repr = Representation { repr: Repr::C, ..representation };

      let bits_ty = Ident::new(&format!("{}Bits", input.ident), input.span());

      let variant_struct = variants
        .iter()
        .map(|v| Ident::new(&format!("{bits_ty}{}", v.ident), v.span()));

      let variant_struct_definitions =
        variant_struct.clone().zip(variants.iter()).map(|(variant_ty, v)| {
          let fields = v.fields.iter().map(|v| &v.ty);

          quote! {
            #[derive(::core::clone::Clone, ::core::marker::Copy, ::bytemuck::CheckedBitPattern)]
            #[repr(C)]
            #vis struct #variant_ty(#integer, #(#fields),*);
          }
        });

      let union_fields =
        variant_struct.clone().zip(variants.iter()).map(|(variant_ty, v)| {
          let variant_ty =
            Ident::new(&format!("{variant_ty}Bits"), input.span());
          let field_ident = &v.ident;
          quote! {
            #field_ident: #variant_ty
          }
        });

      let variant_checks = variant_struct
        .clone()
        .zip(VariantDiscriminantIterator::new(variants.iter()))
        .zip(variants.iter())
        .map(|((variant_ty, discriminant), v)| -> Result<_> {
          let discriminant = discriminant?;
          let discriminant = LitInt::new(&discriminant.to_string(), v.span());
          let ident = &v.ident;
          Ok(quote! {
            #discriminant => {
              let payload = unsafe { &bits.#ident };
              <#variant_ty as ::bytemuck::CheckedBitPattern>::is_valid_bit_pattern(payload)
            }
          })
        })
        .collect::<Result<Vec<_>>>()?;

      Ok((
        quote! {
          #[derive(::core::clone::Clone, ::core::marker::Copy, ::bytemuck::AnyBitPattern)]
          #bits_repr
          #[allow(non_snake_case)]
          #vis union #bits_ty {
            __tag: #integer,
            #(#union_fields,)*
          }

          #[cfg(not(target_arch = "spirv"))]
          impl ::core::fmt::Debug for #bits_ty {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
              let mut debug_struct = ::core::fmt::Formatter::debug_struct(f, ::core::stringify!(#bits_ty));
              ::core::fmt::DebugStruct::field(&mut debug_struct, "tag", unsafe { &self.__tag });
              ::core::fmt::DebugStruct::finish_non_exhaustive(&mut debug_struct)
            }
          }

          #(#variant_struct_definitions)*
        },
        quote! {
          type Bits = #bits_ty;

          #[inline]
          #[allow(clippy::double_comparisons)]
          fn is_valid_bit_pattern(bits: &Self::Bits) -> bool {
            match unsafe { bits.__tag } {
              #(#variant_checks)*
              _ => false,
            }
          }
        },
      ))
    }
  }
}

/// Check that a struct has no padding by asserting that the size of the struct
/// is equal to the sum of the size of it's fields
fn generate_assert_no_padding(input: &DeriveInput) -> Result<TokenStream> {
  let struct_type = &input.ident;
  let span = input.ident.span();
  let fields = get_fields(input)?;

  let mut field_types = get_field_types(&fields);
  let size_sum = if let Some(first) = field_types.next() {
    let size_first = quote_spanned!(span => ::core::mem::size_of::<#first>());
    let size_rest =
      quote_spanned!(span => #( + ::core::mem::size_of::<#field_types>() )*);

    quote_spanned!(span => #size_first#size_rest)
  } else {
    quote_spanned!(span => 0)
  };

  Ok(quote_spanned! {span => const _: fn() = || {
    struct TypeWithoutPadding([u8; #size_sum]);
    let _ = ::core::mem::transmute::<#struct_type, TypeWithoutPadding>;
  };})
}

/// Check that all fields implement a given trait
fn generate_fields_are_trait(
  input: &DeriveInput, trait_: syn::Path,
) -> Result<TokenStream> {
  let (impl_generics, _ty_generics, where_clause) =
    input.generics.split_for_impl();
  let fields = get_fields(input)?;
  let span = input.span();
  let field_types = get_field_types(&fields);
  Ok(quote_spanned! {span => #(const _: fn() = || {
      #[allow(clippy::missing_const_for_fn)]
      fn check #impl_generics () #where_clause {
        fn assert_impl<T: #trait_>() {}
        assert_impl::<#field_types>();
      }
    };)*
  })
}

fn get_ident_from_stream(tokens: TokenStream) -> Option<Ident> {
  match tokens.into_iter().next() {
    Some(TokenTree::Group(group)) => get_ident_from_stream(group.stream()),
    Some(TokenTree::Ident(ident)) => Some(ident),
    _ => None,
  }
}

/// get a simple #[foo(bar)] attribute, returning "bar"
fn get_simple_attr(attributes: &[Attribute], attr_name: &str) -> Option<Ident> {
  for attr in attributes {
    if let (AttrStyle::Outer, Meta::List(list)) = (&attr.style, &attr.meta) {
      if list.path.is_ident(attr_name) {
        if let Some(ident) = get_ident_from_stream(list.tokens.clone()) {
          return Some(ident);
        }
      }
    }
  }

  None
}

fn get_repr(attributes: &[Attribute]) -> Result<Representation> {
  attributes
    .iter()
    .filter_map(|attr| {
      if attr.path().is_ident("repr") {
        Some(attr.parse_args::<Representation>())
      } else {
        None
      }
    })
    .try_fold(Representation::default(), |a, b| {
      let b = b?;
      Ok(Representation {
        repr: match (a.repr, b.repr) {
          (a, Repr::Rust) => a,
          (Repr::Rust, b) => b,
          _ => bail!("conflicting representation hints"),
        },
        packed: match (a.packed, b.packed) {
          (a, None) => a,
          (None, b) => b,
          _ => bail!("conflicting representation hints"),
        },
        align: match (a.align, b.align) {
          (Some(a), Some(b)) => Some(cmp::max(a, b)),
          (a, None) => a,
          (None, b) => b,
        },
      })
    })
}

mk_repr! {
  U8 => u8,
  I8 => i8,
  U16 => u16,
  I16 => i16,
  U32 => u32,
  I32 => i32,
  U64 => u64,
  I64 => i64,
  I128 => i128,
  U128 => u128,
  Usize => usize,
  Isize => isize,
}
// where
macro_rules! mk_repr {(
  $(
    $Xn:ident => $xn:ident
  ),* $(,)?
) => (
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  enum IntegerRepr {
    $($Xn),*
  }

  impl<'a> TryFrom<&'a str> for IntegerRepr {
    type Error = &'a str;

    fn try_from(value: &'a str) -> std::result::Result<Self, &'a str> {
      match value {
        $(
          stringify!($xn) => Ok(Self::$Xn),
        )*
        _ => Err(value),
      }
    }
  }

  impl ToTokens for IntegerRepr {
    fn to_tokens(&self, tokens: &mut TokenStream) {
      match self {
        $(
          Self::$Xn => tokens.extend(quote!($xn)),
        )*
      }
    }
  }
)}
use mk_repr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repr {
  Rust,
  C,
  Transparent,
  Integer(IntegerRepr),
  CWithDiscriminant(IntegerRepr),
}

impl Repr {
  fn is_integer(&self) -> bool {
    matches!(self, Self::Integer(..))
  }

  fn as_integer(&self) -> Option<IntegerRepr> {
    if let Self::Integer(v) = self {
      Some(*v)
    } else {
      None
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Representation {
  packed: Option<u32>,
  align: Option<u32>,
  repr: Repr,
}

impl Default for Representation {
  fn default() -> Self {
    Self { packed: None, align: None, repr: Repr::Rust }
  }
}

impl Parse for Representation {
  fn parse(input: ParseStream<'_>) -> Result<Representation> {
    let mut ret = Representation::default();
    while !input.is_empty() {
      let keyword = input.parse::<Ident>()?;
      // preëmptively call `.to_string()` *once* (rather than on `is_ident()`)
      let keyword_str = keyword.to_string();
      let new_repr = match keyword_str.as_str() {
        "C" => Repr::C,
        "transparent" => Repr::Transparent,
        "packed" => {
          ret.packed = Some(if input.peek(token::Paren) {
            let contents;
            parenthesized!(contents in input);
            LitInt::base10_parse::<u32>(&contents.parse()?)?
          } else {
            1
          });
          let _: Option<Token![,]> = input.parse()?;
          continue;
        }
        "align" => {
          let contents;
          parenthesized!(contents in input);
          let new_align = LitInt::base10_parse::<u32>(&contents.parse()?)?;
          ret.align = Some(
            ret
              .align
              .map_or(new_align, |old_align| cmp::max(old_align, new_align)),
          );
          let _: Option<Token![,]> = input.parse()?;
          continue;
        }
        ident => {
          let primitive = IntegerRepr::try_from(ident)
            .map_err(|_| input.error("unrecognized representation hint"))?;
          Repr::Integer(primitive)
        }
      };
      ret.repr = match (ret.repr, new_repr) {
        (Repr::Rust, new_repr) => {
          // This is the first explicit repr.
          new_repr
        }
        (Repr::C, Repr::Integer(integer))
        | (Repr::Integer(integer), Repr::C) => {
          // Both the C repr and an integer repr have been specified
          // -> merge into a C wit discriminant.
          Repr::CWithDiscriminant(integer)
        }
        (_, _) => {
          return Err(input.error("duplicate representation hint"));
        }
      };
      let _: Option<Token![,]> = input.parse()?;
    }
    Ok(ret)
  }
}

impl ToTokens for Representation {
  fn to_tokens(&self, tokens: &mut TokenStream) {
    let mut meta = Punctuated::<_, Token![,]>::new();

    match self.repr {
      Repr::Rust => {}
      Repr::C => meta.push(quote!(C)),
      Repr::Transparent => meta.push(quote!(transparent)),
      Repr::Integer(primitive) => meta.push(quote!(#primitive)),
      Repr::CWithDiscriminant(primitive) => {
        meta.push(quote!(C));
        meta.push(quote!(#primitive));
      }
    }

    if let Some(packed) = self.packed.as_ref() {
      let lit = LitInt::new(&packed.to_string(), Span::call_site());
      meta.push(quote!(packed(#lit)));
    }

    if let Some(align) = self.align.as_ref() {
      let lit = LitInt::new(&align.to_string(), Span::call_site());
      meta.push(quote!(align(#lit)));
    }

    tokens.extend(quote!(
      #[repr(#meta)]
    ));
  }
}

fn enum_has_fields<'a>(
  mut variants: impl Iterator<Item = &'a Variant>,
) -> bool {
  variants.any(|v| matches!(v.fields, Fields::Named(_) | Fields::Unnamed(_)))
}

struct VariantDiscriminantIterator<'a, I: Iterator<Item = &'a Variant> + 'a> {
  inner: I,
  last_value: i64,
}

impl<'a, I: Iterator<Item = &'a Variant> + 'a>
  VariantDiscriminantIterator<'a, I>
{
  fn new(inner: I) -> Self {
    VariantDiscriminantIterator { inner, last_value: -1 }
  }
}

impl<'a, I: Iterator<Item = &'a Variant> + 'a> Iterator
  for VariantDiscriminantIterator<'a, I>
{
  type Item = Result<i64>;

  fn next(&mut self) -> Option<Self::Item> {
    let variant = self.inner.next()?;

    if let Some((_, discriminant)) = &variant.discriminant {
      let discriminant_value = match parse_int_expr(discriminant) {
        Ok(value) => value,
        Err(e) => return Some(Err(e)),
      };
      self.last_value = discriminant_value;
    } else {
      self.last_value += 1;
    }

    Some(Ok(self.last_value))
  }
}

fn parse_int_expr(expr: &Expr) -> Result<i64> {
  match expr {
    Expr::Unary(ExprUnary { op: UnOp::Neg(_), expr, .. }) => {
      parse_int_expr(expr).map(|int| -int)
    }
    Expr::Lit(ExprLit { lit: Lit::Int(int), .. }) => int.base10_parse(),
    Expr::Lit(ExprLit { lit: Lit::Byte(byte), .. }) => Ok(byte.value().into()),
    _ => bail!("Not an integer expression"),
  }
}

#[cfg(test)]
mod tests {
  use syn::parse_quote;

  use super::{get_repr, IntegerRepr, Repr, Representation};

  #[test]
  fn parse_basic_repr() {
    let attr = parse_quote!(#[repr(C)]);
    let repr = get_repr(&[attr]).unwrap();
    assert_eq!(repr, Representation { repr: Repr::C, ..Default::default() });

    let attr = parse_quote!(#[repr(transparent)]);
    let repr = get_repr(&[attr]).unwrap();
    assert_eq!(
      repr,
      Representation { repr: Repr::Transparent, ..Default::default() }
    );

    let attr = parse_quote!(#[repr(u8)]);
    let repr = get_repr(&[attr]).unwrap();
    assert_eq!(
      repr,
      Representation {
        repr: Repr::Integer(IntegerRepr::U8),
        ..Default::default()
      }
    );

    let attr = parse_quote!(#[repr(packed)]);
    let repr = get_repr(&[attr]).unwrap();
    assert_eq!(repr, Representation { packed: Some(1), ..Default::default() });

    let attr = parse_quote!(#[repr(packed(1))]);
    let repr = get_repr(&[attr]).unwrap();
    assert_eq!(repr, Representation { packed: Some(1), ..Default::default() });

    let attr = parse_quote!(#[repr(packed(2))]);
    let repr = get_repr(&[attr]).unwrap();
    assert_eq!(repr, Representation { packed: Some(2), ..Default::default() });

    let attr = parse_quote!(#[repr(align(2))]);
    let repr = get_repr(&[attr]).unwrap();
    assert_eq!(repr, Representation { align: Some(2), ..Default::default() });
  }

  #[test]
  fn parse_advanced_repr() {
    let attr = parse_quote!(#[repr(align(4), align(2))]);
    let repr = get_repr(&[attr]).unwrap();
    assert_eq!(repr, Representation { align: Some(4), ..Default::default() });

    let attr1 = parse_quote!(#[repr(align(1))]);
    let attr2 = parse_quote!(#[repr(align(4))]);
    let attr3 = parse_quote!(#[repr(align(2))]);
    let repr = get_repr(&[attr1, attr2, attr3]).unwrap();
    assert_eq!(repr, Representation { align: Some(4), ..Default::default() });

    let attr = parse_quote!(#[repr(C, u8)]);
    let repr = get_repr(&[attr]).unwrap();
    assert_eq!(
      repr,
      Representation {
        repr: Repr::CWithDiscriminant(IntegerRepr::U8),
        ..Default::default()
      }
    );

    let attr = parse_quote!(#[repr(u8, C)]);
    let repr = get_repr(&[attr]).unwrap();
    assert_eq!(
      repr,
      Representation {
        repr: Repr::CWithDiscriminant(IntegerRepr::U8),
        ..Default::default()
      }
    );
  }
}

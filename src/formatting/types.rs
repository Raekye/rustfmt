use std::iter::ExactSizeIterator;
use std::ops::Deref;

use rustc_ast::ast::{self, FnRetTy, Mutability};
use rustc_span::{symbol::kw, symbol::Ident, BytePos, Pos, Span};

use crate::config::{lists::*, IndentStyle, TypeDensity};
use crate::formatting::{
    comment::{combine_strs_with_missing_comments, contains_comment},
    expr::{
        format_expr, rewrite_assign_rhs, rewrite_call, rewrite_tuple, rewrite_unary_prefix,
        ExprType,
    },
    lists::{definitive_tactic, itemize_list, write_list, ListFormatting, ListItem, Separator},
    macros::{rewrite_macro, MacroPosition},
    overflow,
    pairs::{rewrite_pair, PairParts},
    rewrite::{Rewrite, RewriteContext},
    shape::Shape,
    source_map::SpanUtils,
    spanned::Spanned,
    utils::{
        colon_spaces, extra_offset, first_line_width, format_extern, format_mutability,
        format_unsafety, last_line_extendable, last_line_width, mk_sp, rewrite_ident,
    },
};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum PathContext {
    Expr,
    Type,
    Import,
}

// Does not wrap on simple segments.
pub(crate) fn rewrite_path(
    context: &RewriteContext<'_>,
    path_context: PathContext,
    qself: Option<&ast::QSelf>,
    path: &ast::Path,
    shape: Shape,
) -> Option<String> {
    let skip_count = qself.map_or(0, |x| x.position);

    let mut result = if path.is_global() && qself.is_none() && path_context != PathContext::Import {
        "::".to_owned()
    } else {
        String::new()
    };

    let mut span_lo = path.span.lo();

    if let Some(qself) = qself {
        result.push('<');

        let fmt_ty = qself.ty.rewrite(context, shape)?;
        result.push_str(&fmt_ty);

        if skip_count > 0 {
            result.push_str(" as ");
            if path.is_global() && path_context != PathContext::Import {
                result.push_str("::");
            }

            // 3 = ">::".len()
            let shape = shape.sub_width(3)?;

            result = rewrite_path_segments(
                PathContext::Type,
                result,
                path.segments.iter().take(skip_count),
                span_lo,
                path.span.hi(),
                context,
                shape,
            )?;
        }

        result.push_str(">::");
        span_lo = qself.ty.span.hi() + BytePos(1);
    }

    rewrite_path_segments(
        path_context,
        result,
        path.segments.iter().skip(skip_count),
        span_lo,
        path.span.hi(),
        context,
        shape,
    )
}

fn rewrite_path_segments<'a, I>(
    path_context: PathContext,
    mut buffer: String,
    iter: I,
    mut span_lo: BytePos,
    span_hi: BytePos,
    context: &RewriteContext<'_>,
    shape: Shape,
) -> Option<String>
where
    I: Iterator<Item = &'a ast::PathSegment>,
{
    let mut first = true;
    let shape = shape.visual_indent(0);

    for segment in iter {
        // Indicates a global path, shouldn't be rendered.
        if segment.ident.name == kw::PathRoot {
            continue;
        }
        if first {
            first = false;
        } else {
            buffer.push_str("::");
        }

        let extra_offset = extra_offset(&buffer, shape);
        let new_shape = shape.shrink_left(extra_offset)?;
        let segment_string = rewrite_segment(
            path_context,
            segment,
            &mut span_lo,
            span_hi,
            context,
            new_shape,
        )?;

        buffer.push_str(&segment_string);
    }

    Some(buffer)
}

#[derive(Debug)]
pub(crate) enum SegmentParam<'a> {
    Const(&'a ast::AnonConst),
    LifeTime(&'a ast::Lifetime),
    Type(&'a ast::Ty),
    Binding(&'a ast::AssocTyConstraint),
}

impl<'a> SegmentParam<'a> {
    fn from_generic_arg(arg: &ast::GenericArg) -> SegmentParam<'_> {
        match arg {
            ast::GenericArg::Lifetime(ref lt) => SegmentParam::LifeTime(lt),
            ast::GenericArg::Type(ref ty) => SegmentParam::Type(ty),
            ast::GenericArg::Const(const_) => SegmentParam::Const(const_),
        }
    }
}

impl<'a> Spanned for SegmentParam<'a> {
    fn span(&self) -> Span {
        match *self {
            SegmentParam::Const(const_) => const_.value.span,
            SegmentParam::LifeTime(lt) => lt.ident.span,
            SegmentParam::Type(ty) => ty.span,
            SegmentParam::Binding(binding) => binding.span,
        }
    }
}

impl<'a> Rewrite for SegmentParam<'a> {
    fn rewrite(&self, context: &RewriteContext<'_>, shape: Shape) -> Option<String> {
        match *self {
            SegmentParam::Const(const_) => const_.rewrite(context, shape),
            SegmentParam::LifeTime(lt) => lt.rewrite(context, shape),
            SegmentParam::Type(ty) => ty.rewrite(context, shape),
            SegmentParam::Binding(assoc_ty_constraint) => {
                let ident = rewrite_ident(context, assoc_ty_constraint.ident);
                let segment = if let Some(ref gen_args) = assoc_ty_constraint.gen_args {
                    append_rewrite_args(
                        PathContext::Type,
                        assoc_ty_constraint.ident,
                        gen_args,
                        &mut assoc_ty_constraint.span.lo(),
                        assoc_ty_constraint.span.hi(),
                        context,
                        shape,
                        ident.to_string(),
                    )?
                } else {
                    ident.to_string()
                };
                let mut result = match assoc_ty_constraint.kind {
                    ast::AssocTyConstraintKind::Bound { .. } => {
                        format!("{}: ", segment)
                    }
                    ast::AssocTyConstraintKind::Equality { .. } => {
                        match context.config.type_punctuation_density() {
                            TypeDensity::Wide => {
                                format!("{} = ", segment)
                            }
                            TypeDensity::Compressed => {
                                format!("{}=", segment)
                            }
                        }
                    }
                };

                let budget = shape.width.checked_sub(result.len())?;
                let rewrite = assoc_ty_constraint
                    .kind
                    .rewrite(context, Shape::legacy(budget, shape.indent + result.len()))?;
                result.push_str(&rewrite);
                Some(result)
            }
        }
    }
}

impl Rewrite for ast::AssocTyConstraintKind {
    fn rewrite(&self, context: &RewriteContext<'_>, shape: Shape) -> Option<String> {
        match self {
            ast::AssocTyConstraintKind::Equality { ty } => ty.rewrite(context, shape),
            ast::AssocTyConstraintKind::Bound { bounds } => bounds.rewrite(context, shape),
        }
    }
}

// Formats a path segment. There are some hacks involved to correctly determine
// the segment's associated span since it's not part of the AST.
//
// The span_lo is assumed to be greater than the end of any previous segment's
// parameters and lesser or equal than the start of current segment.
//
// span_hi is assumed equal to the end of the entire path.
//
// When the segment contains a positive number of parameters, we update span_lo
// so that invariants described above will hold for the next segment.
fn rewrite_segment(
    path_context: PathContext,
    segment: &ast::PathSegment,
    span_lo: &mut BytePos,
    span_hi: BytePos,
    context: &RewriteContext<'_>,
    shape: Shape,
) -> Option<String> {
    let mut result = String::with_capacity(128);
    result.push_str(rewrite_ident(context, segment.ident));

    let ident_len = result.len();
    let shape = if context.use_block_indent() {
        shape.offset_left(ident_len)?
    } else {
        shape.shrink_left(ident_len)?
    };

    if let Some(ref args) = segment.args {
        append_rewrite_args(
            path_context,
            segment.ident,
            args,
            span_lo,
            span_hi,
            context,
            shape,
            result,
        )
    } else {
        Some(result)
    }
}

fn append_rewrite_args(
    path_context: PathContext,
    ident: Ident,
    args: &ast::GenericArgs,
    span_lo: &mut BytePos,
    span_hi: BytePos,
    context: &RewriteContext<'_>,
    shape: Shape,
    mut result: String,
) -> Option<String> {
    match args {
        ast::GenericArgs::AngleBracketed(ref data) if !data.args.is_empty() => {
            let param_list = data
                .args
                .iter()
                .map(|x| match x {
                    ast::AngleBracketedArg::Arg(generic_arg) => {
                        SegmentParam::from_generic_arg(generic_arg)
                    }
                    ast::AngleBracketedArg::Constraint(constraint) => {
                        SegmentParam::Binding(constraint)
                    }
                })
                .collect::<Vec<_>>();

            // HACK: squeeze out the span between the identifier and the parameters.
            // The hack is requried so that we don't remove the separator inside macro calls.
            // This does not work in the presence of comment, hoping that people are
            // sane about where to put their comment.
            let separator_snippet = context
                .snippet(mk_sp(ident.span.hi(), data.span.lo()))
                .trim();
            let force_separator = context.inside_macro() && separator_snippet.starts_with("::");
            let separator = if path_context == PathContext::Expr || force_separator {
                "::"
            } else {
                ""
            };
            result.push_str(separator);

            let generics_str = overflow::rewrite_with_angle_brackets(
                context,
                "",
                param_list.iter(),
                shape,
                mk_sp(*span_lo, span_hi),
            )?;

            // Update position of last bracket.
            *span_lo = context
                .snippet_provider
                .span_after(mk_sp(*span_lo, span_hi), "<");

            result.push_str(&generics_str)
        }
        ast::GenericArgs::Parenthesized(ref data) => {
            result.push_str(&format_function_type(
                data.inputs.iter().map(|x| &**x),
                &data.output,
                false,
                data.span,
                context,
                shape,
            )?);
        }
        _ => {}
    }

    Some(result)
}

fn format_function_type<'a, I>(
    inputs: I,
    output: &FnRetTy,
    variadic: bool,
    span: Span,
    context: &RewriteContext<'_>,
    shape: Shape,
) -> Option<String>
where
    I: ExactSizeIterator,
    <I as Iterator>::Item: Deref,
    <I::Item as Deref>::Target: Rewrite + Spanned + 'a,
{
    debug!("format_function_type {:#?}", shape);

    let ty_shape = match context.config.indent_style() {
        // 4 = " -> "
        IndentStyle::Block => shape.offset_left(4)?,
        IndentStyle::Visual => shape.block_left(4)?,
    };
    let output = match *output {
        FnRetTy::Ty(ref ty) => {
            let type_str = ty.rewrite(context, ty_shape)?;
            format!(" -> {}", type_str)
        }
        FnRetTy::Default(..) => String::new(),
    };

    let list_shape = if context.use_block_indent() {
        Shape::indented(
            shape.block().indent.block_indent(context.config),
            context.config,
        )
    } else {
        // 2 for ()
        let budget = shape.width.checked_sub(2)?;
        // 1 for (
        let offset = shape.indent + 1;
        Shape::legacy(budget, offset)
    };

    let is_inputs_empty = inputs.len() == 0;
    let list_lo = context.snippet_provider.span_after(span, "(");
    let (list_str, tactic) = if is_inputs_empty {
        let tactic = get_tactics(&[], &output, shape);
        let list_hi = context.snippet_provider.span_before(span, ")");
        let comment = context
            .snippet_provider
            .span_to_snippet(mk_sp(list_lo, list_hi))?
            .trim();
        let comment = if comment.starts_with("//") {
            format!(
                "{}{}{}",
                &list_shape.indent.to_string_with_newline(context.config),
                comment,
                &shape.block().indent.to_string_with_newline(context.config)
            )
        } else {
            comment.to_string()
        };
        (comment, tactic)
    } else {
        let items = itemize_list(
            context.snippet_provider,
            inputs,
            ")",
            ",",
            |arg| arg.span().lo(),
            |arg| arg.span().hi(),
            |arg| arg.rewrite(context, list_shape),
            list_lo,
            span.hi(),
            false,
        );

        let item_vec: Vec<_> = items.collect();
        let tactic = get_tactics(&item_vec, &output, shape);
        let trailing_separator = if !context.use_block_indent() || variadic {
            SeparatorTactic::Never
        } else {
            context.config.trailing_comma()
        };

        let fmt = ListFormatting::new(list_shape, context.config)
            .tactic(tactic)
            .trailing_separator(trailing_separator)
            .ends_with_newline(tactic.ends_with_newline(context.config.indent_style()))
            .preserve_newline(true);
        (write_list(&item_vec, &fmt)?, tactic)
    };

    let args = if tactic == DefinitiveListTactic::Horizontal
        || !context.use_block_indent()
        || is_inputs_empty
    {
        format!("({})", list_str)
    } else {
        format!(
            "({}{}{})",
            list_shape.indent.to_string_with_newline(context.config),
            list_str,
            shape.block().indent.to_string_with_newline(context.config),
        )
    };
    if output.is_empty() || last_line_width(&args) + first_line_width(&output) <= shape.width {
        Some(format!("{}{}", args, output))
    } else {
        Some(format!(
            "{}\n{}{}",
            args,
            list_shape.indent.to_string(context.config),
            output.trim_start()
        ))
    }
}

fn type_bound_colon(context: &RewriteContext<'_>) -> &'static str {
    colon_spaces(context.config)
}

// If the return type is multi-lined, then force to use multiple lines for
// arguments as well.
fn get_tactics(item_vec: &[ListItem], output: &str, shape: Shape) -> DefinitiveListTactic {
    if output.contains('\n') {
        DefinitiveListTactic::Vertical
    } else {
        definitive_tactic(
            item_vec,
            ListTactic::HorizontalVertical,
            Separator::Comma,
            // 2 is for the case of ',\n'
            shape.width.saturating_sub(2 + output.len()),
        )
    }
}

impl Rewrite for ast::WherePredicate {
    fn rewrite(&self, context: &RewriteContext<'_>, shape: Shape) -> Option<String> {
        // FIXME: dead spans?
        let result = match *self {
            ast::WherePredicate::BoundPredicate(ast::WhereBoundPredicate {
                ref bound_generic_params,
                ref bounded_ty,
                ref bounds,
                ..
            }) => {
                let type_str = bounded_ty.rewrite(context, shape)?;
                let colon = type_bound_colon(context).trim_end();
                let lhs = if let Some(lifetime_str) =
                    rewrite_generic_params(context, shape, bound_generic_params)
                {
                    format!("for<{}> {}{}", lifetime_str, type_str, colon)
                } else {
                    format!("{}{}", type_str, colon)
                };

                rewrite_assign_rhs(context, lhs, bounds, shape)?
            }
            ast::WherePredicate::RegionPredicate(ast::WhereRegionPredicate {
                ref lifetime,
                ref bounds,
                ..
            }) => rewrite_bounded_lifetime(lifetime, bounds, context, shape)?,
            ast::WherePredicate::EqPredicate(ast::WhereEqPredicate {
                ref lhs_ty,
                ref rhs_ty,
                ..
            }) => {
                let lhs_ty_str = lhs_ty.rewrite(context, shape).map(|lhs| lhs + " =")?;
                rewrite_assign_rhs(context, lhs_ty_str, &**rhs_ty, shape)?
            }
        };

        Some(result)
    }
}

impl Rewrite for ast::GenericArg {
    fn rewrite(&self, context: &RewriteContext<'_>, shape: Shape) -> Option<String> {
        match *self {
            ast::GenericArg::Lifetime(ref lt) => lt.rewrite(context, shape),
            ast::GenericArg::Type(ref ty) => ty.rewrite(context, shape),
            ast::GenericArg::Const(ref const_) => const_.rewrite(context, shape),
        }
    }
}

fn rewrite_bounded_lifetime(
    lt: &ast::Lifetime,
    bounds: &[ast::GenericBound],
    context: &RewriteContext<'_>,
    shape: Shape,
) -> Option<String> {
    let result = lt.rewrite(context, shape)?;

    if bounds.is_empty() {
        Some(result)
    } else {
        let colon = type_bound_colon(context);
        let overhead = last_line_width(&result) + colon.len();
        let result = format!(
            "{}{}{}",
            result,
            colon,
            join_bounds(context, shape.sub_width(overhead)?, bounds, true)?
        );
        Some(result)
    }
}

impl Rewrite for ast::AnonConst {
    fn rewrite(&self, context: &RewriteContext<'_>, shape: Shape) -> Option<String> {
        format_expr(&self.value, ExprType::SubExpression, context, shape)
    }
}

impl Rewrite for ast::Lifetime {
    fn rewrite(&self, context: &RewriteContext<'_>, _: Shape) -> Option<String> {
        Some(rewrite_ident(context, self.ident).to_owned())
    }
}

impl Rewrite for ast::GenericBound {
    fn rewrite(&self, context: &RewriteContext<'_>, shape: Shape) -> Option<String> {
        match *self {
            ast::GenericBound::Trait(ref poly_trait_ref, trait_bound_modifier) => {
                let snippet = context.snippet(self.span());
                let has_paren = snippet.starts_with('(') && snippet.ends_with(')');
                let rewrite = match trait_bound_modifier {
                    ast::TraitBoundModifier::None => poly_trait_ref.rewrite(context, shape),
                    ast::TraitBoundModifier::Maybe => poly_trait_ref
                        .rewrite(context, shape.offset_left(1)?)
                        .map(|s| format!("?{}", s)),
                    ast::TraitBoundModifier::MaybeConst => poly_trait_ref
                        .rewrite(context, shape.offset_left(7)?)
                        .map(|s| format!("?const {}", s)),
                    ast::TraitBoundModifier::MaybeConstMaybe => poly_trait_ref
                        .rewrite(context, shape.offset_left(8)?)
                        .map(|s| format!("?const ?{}", s)),
                };
                rewrite.map(|s| if has_paren { format!("({})", s) } else { s })
            }
            ast::GenericBound::Outlives(ref lifetime) => lifetime.rewrite(context, shape),
        }
    }
}

impl Rewrite for ast::GenericBounds {
    fn rewrite(&self, context: &RewriteContext<'_>, shape: Shape) -> Option<String> {
        if self.is_empty() {
            return Some(String::new());
        }

        join_bounds(context, shape, self, true)
    }
}

impl Rewrite for ast::GenericParam {
    fn rewrite(&self, context: &RewriteContext<'_>, shape: Shape) -> Option<String> {
        let mut result = String::with_capacity(128);
        // FIXME: If there are more than one attributes, this will force multiline.
        match self.attrs.rewrite(context, shape) {
            Some(ref rw) if !rw.is_empty() => result.push_str(&format!("{} ", rw)),
            _ => {}
        }

        if let ast::GenericParamKind::Const {
            ref ty,
            kw_span: _,
            default,
        } = &self.kind
        {
            result.push_str("const ");
            result.push_str(rewrite_ident(context, self.ident));
            result.push_str(": ");
            result.push_str(&ty.rewrite(context, shape)?);
            if let Some(default) = default {
                let eq_str = match context.config.type_punctuation_density() {
                    TypeDensity::Compressed => "=",
                    TypeDensity::Wide => " = ",
                };
                result.push_str(eq_str);
                let budget = shape.width.checked_sub(result.len())?;
                let rewrite = default.rewrite(context, Shape::legacy(budget, shape.indent))?;
                result.push_str(&rewrite);
            }
        } else {
            result.push_str(rewrite_ident(context, self.ident));
        }

        if !self.bounds.is_empty() {
            result.push_str(type_bound_colon(context));
            result.push_str(&self.bounds.rewrite(context, shape)?)
        }
        if let ast::GenericParamKind::Type {
            default: Some(ref def),
        } = self.kind
        {
            let eq_str = match context.config.type_punctuation_density() {
                TypeDensity::Compressed => "=",
                TypeDensity::Wide => " = ",
            };
            result.push_str(eq_str);
            let budget = shape.width.checked_sub(result.len())?;
            let rewrite =
                def.rewrite(context, Shape::legacy(budget, shape.indent + result.len()))?;
            result.push_str(&rewrite);
        }

        Some(result)
    }
}

impl Rewrite for ast::PolyTraitRef {
    fn rewrite(&self, context: &RewriteContext<'_>, shape: Shape) -> Option<String> {
        if let Some(lifetime_str) =
            rewrite_generic_params(context, shape, &self.bound_generic_params)
        {
            // 6 is "for<> ".len()
            let extra_offset = lifetime_str.len() + 6;
            let path_str = self
                .trait_ref
                .rewrite(context, shape.offset_left(extra_offset)?)?;

            Some(format!("for<{}> {}", lifetime_str, path_str))
        } else {
            self.trait_ref.rewrite(context, shape)
        }
    }
}

impl Rewrite for ast::TraitRef {
    fn rewrite(&self, context: &RewriteContext<'_>, shape: Shape) -> Option<String> {
        rewrite_path(context, PathContext::Type, None, &self.path, shape)
    }
}

impl Rewrite for ast::Ty {
    fn rewrite(&self, context: &RewriteContext<'_>, shape: Shape) -> Option<String> {
        match self.kind {
            ast::TyKind::TraitObject(ref bounds, tobj_syntax) => {
                // we have to consider 'dyn' keyword is used or not!!!
                let is_dyn = tobj_syntax == ast::TraitObjectSyntax::Dyn;
                // 4 is length of 'dyn '
                let shape = if is_dyn { shape.offset_left(4)? } else { shape };
                let mut res = bounds.rewrite(context, shape)?;
                // We may have falsely removed a trailing `+` inside macro call.
                if context.inside_macro()
                    && bounds.len() == 1
                    && context.snippet(self.span).ends_with('+')
                    && !res.ends_with('+')
                {
                    res.push('+');
                }
                if is_dyn {
                    Some(format!("dyn {}", res))
                } else {
                    Some(res)
                }
            }
            ast::TyKind::Ptr(ref mt) => {
                let prefix = match mt.mutbl {
                    Mutability::Mut => "*mut ",
                    Mutability::Not => "*const ",
                };

                rewrite_unary_prefix(context, prefix, &*mt.ty, shape)
            }
            ast::TyKind::Rptr(ref lifetime, ref mt) => {
                let mut_str = format_mutability(mt.mutbl);
                let mut_len = mut_str.len();
                let mut result = String::with_capacity(128);
                result.push('&');
                let ref_hi = context.snippet_provider.span_after(self.span(), "&");
                let mut cmnt_lo = ref_hi;

                if let Some(ref lifetime) = *lifetime {
                    let lt_budget = shape.width.checked_sub(2 + mut_len)?;
                    let lt_str = lifetime.rewrite(
                        context,
                        Shape::legacy(lt_budget, shape.indent + 2 + mut_len),
                    )?;
                    let before_lt_span = mk_sp(cmnt_lo, lifetime.ident.span.lo());
                    if contains_comment(context.snippet(before_lt_span)) {
                        result = combine_strs_with_missing_comments(
                            context,
                            &result,
                            &lt_str,
                            before_lt_span,
                            shape,
                            true,
                        )?;
                    } else {
                        result.push_str(&lt_str);
                    }
                    result.push(' ');
                    cmnt_lo = lifetime.ident.span.hi();
                }

                if ast::Mutability::Mut == mt.mutbl {
                    let mut_hi = context.snippet_provider.span_after(self.span(), "mut");
                    let before_mut_span = mk_sp(cmnt_lo, mut_hi - BytePos::from_usize(3));
                    if contains_comment(context.snippet(before_mut_span)) {
                        result = combine_strs_with_missing_comments(
                            context,
                            result.trim_end(),
                            mut_str,
                            before_mut_span,
                            shape,
                            true,
                        )?;
                    } else {
                        result.push_str(mut_str);
                    }
                    cmnt_lo = mut_hi;
                }

                let before_ty_span = mk_sp(cmnt_lo, mt.ty.span.lo());
                if contains_comment(context.snippet(before_ty_span)) {
                    result = combine_strs_with_missing_comments(
                        context,
                        result.trim_end(),
                        &mt.ty.rewrite(&context, shape)?,
                        before_ty_span,
                        shape,
                        true,
                    )?;
                } else {
                    let used_width = last_line_width(&result);
                    let budget = shape.width.checked_sub(used_width)?;
                    let ty_str = mt
                        .ty
                        .rewrite(&context, Shape::legacy(budget, shape.indent + used_width))?;
                    result.push_str(&ty_str);
                }

                Some(result)
            }
            // FIXME: we drop any comments here, even though it's a silly place to put
            // comments.
            ast::TyKind::Paren(ref ty) => {
                if context.config.indent_style() == IndentStyle::Visual {
                    let budget = shape.width.checked_sub(2)?;
                    return ty
                        .rewrite(context, Shape::legacy(budget, shape.indent + 1))
                        .map(|ty_str| format!("({})", ty_str));
                }

                // 2 = ()
                if let Some(sh) = shape.sub_width(2) {
                    if let Some(ref s) = ty.rewrite(context, sh) {
                        if !s.contains('\n') {
                            return Some(format!("({})", s));
                        }
                    }
                }

                let indent_str = shape.indent.to_string_with_newline(context.config);
                let shape = shape
                    .block_indent(context.config.tab_spaces())
                    .with_max_width(context.config);
                let rw = ty.rewrite(context, shape)?;
                Some(format!(
                    "({}{}{})",
                    shape.to_string_with_newline(context.config),
                    rw,
                    indent_str
                ))
            }
            ast::TyKind::Slice(ref ty) => {
                let budget = shape.width.checked_sub(4)?;
                ty.rewrite(context, Shape::legacy(budget, shape.indent + 1))
                    .map(|ty_str| format!("[{}]", ty_str))
            }
            ast::TyKind::Tup(ref items) => {
                rewrite_tuple(context, items.iter(), self.span, shape, items.len() == 1)
            }
            ast::TyKind::Path(ref q_self, ref path) => {
                rewrite_path(context, PathContext::Type, q_self.as_ref(), path, shape)
            }
            ast::TyKind::Array(ref ty, ref repeats) => rewrite_pair(
                &**ty,
                &*repeats.value,
                PairParts::new("[", "", "; ", "", "]"),
                context,
                shape,
                SeparatorPlace::Back,
            ),
            ast::TyKind::Infer => {
                if shape.width >= 1 {
                    Some("_".to_owned())
                } else {
                    None
                }
            }
            ast::TyKind::BareFn(ref bare_fn) => rewrite_bare_fn(bare_fn, self.span, context, shape),
            ast::TyKind::Never => Some(String::from("!")),
            ast::TyKind::MacCall(ref mac) => {
                rewrite_macro(mac, None, context, shape, MacroPosition::Expression)
            }
            ast::TyKind::ImplicitSelf => Some(String::from("")),
            ast::TyKind::ImplTrait(_, ref it) => {
                // Empty trait is not a parser error.
                if it.is_empty() {
                    return Some("impl".to_owned());
                }
                let rw = join_bounds(context, shape, it, false);
                rw.map(|it_str| {
                    let space = if it_str.is_empty() { "" } else { " " };
                    format!("impl{}{}", space, it_str)
                })
            }
            ast::TyKind::CVarArgs => Some("...".to_owned()),
            ast::TyKind::Err => Some(context.snippet(self.span).to_owned()),
            ast::TyKind::Typeof(ref anon_const) => rewrite_call(
                context,
                "typeof",
                &[anon_const.value.clone()],
                self.span,
                shape,
            ),
        }
    }
}

fn rewrite_bare_fn(
    bare_fn: &ast::BareFnTy,
    span: Span,
    context: &RewriteContext<'_>,
    shape: Shape,
) -> Option<String> {
    debug!("rewrite_bare_fn {:#?}", shape);

    let mut result = String::with_capacity(128);

    if let Some(ref lifetime_str) = rewrite_generic_params(context, shape, &bare_fn.generic_params)
    {
        result.push_str("for<");
        // 6 = "for<> ".len(), 4 = "for<".
        // This doesn't work out so nicely for multiline situation with lots of
        // rightward drift. If that is a problem, we could use the list stuff.
        result.push_str(lifetime_str);
        result.push_str("> ");
    }

    result.push_str(format_unsafety(bare_fn.unsafety));

    result.push_str(&format_extern(
        bare_fn.ext,
        context.config.force_explicit_abi(),
        false,
        None,
    ));

    result.push_str("fn");

    let func_ty_shape = if context.use_block_indent() {
        shape.offset_left(result.len())?
    } else {
        shape.visual_indent(result.len()).sub_width(result.len())?
    };

    let rewrite = format_function_type(
        bare_fn.decl.inputs.iter(),
        &bare_fn.decl.output,
        bare_fn.decl.c_variadic(),
        span,
        context,
        func_ty_shape,
    )?;

    result.push_str(&rewrite);

    Some(result)
}

fn is_generic_bounds_in_order(generic_bounds: &[ast::GenericBound]) -> bool {
    let is_trait = |b: &ast::GenericBound| match b {
        ast::GenericBound::Outlives(..) => false,
        ast::GenericBound::Trait(..) => true,
    };
    let is_lifetime = |b: &ast::GenericBound| !is_trait(b);
    let last_trait_index = generic_bounds.iter().rposition(is_trait);
    let first_lifetime_index = generic_bounds.iter().position(is_lifetime);
    match (last_trait_index, first_lifetime_index) {
        (Some(last_trait_index), Some(first_lifetime_index)) => {
            last_trait_index < first_lifetime_index
        }
        _ => true,
    }
}

fn join_bounds(
    context: &RewriteContext<'_>,
    shape: Shape,
    items: &[ast::GenericBound],
    need_indent: bool,
) -> Option<String> {
    join_bounds_inner(context, shape, items, need_indent, false)
}

fn join_bounds_inner(
    context: &RewriteContext<'_>,
    shape: Shape,
    items: &[ast::GenericBound],
    need_indent: bool,
    force_newline: bool,
) -> Option<String> {
    debug_assert!(!items.is_empty());

    let generic_bounds_in_order = is_generic_bounds_in_order(items);
    let is_bound_extendable = |s: &str, b: &ast::GenericBound| match b {
        ast::GenericBound::Outlives(..) => true,
        ast::GenericBound::Trait(..) => last_line_extendable(s),
    };

    // Whether a PathSegment segment includes internal array containing more than one item
    let is_segment_with_multi_items_array = |seg: &ast::PathSegment| {
        if let Some(args_in) = &seg.args {
            match &**args_in {
                ast::AngleBracketed(args) => {
                    if args.args.len() > 1 {
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            }
        } else {
            false
        }
    };

    let result = items.iter().enumerate().try_fold(
        (String::new(), None, false),
        |(strs, prev_trailing_span, prev_extendable), (i, item)| {
            let trailing_span = if i < items.len() - 1 {
                let hi = context
                    .snippet_provider
                    .span_before(mk_sp(items[i + 1].span().lo(), item.span().hi()), "+");

                Some(mk_sp(item.span().hi(), hi))
            } else {
                None
            };
            let (leading_span, has_leading_comment) = if i > 0 {
                let lo = context
                    .snippet_provider
                    .span_after(mk_sp(items[i - 1].span().hi(), item.span().lo()), "+");

                let span = mk_sp(lo, item.span().lo());

                let has_comments = contains_comment(context.snippet(span));

                (Some(mk_sp(lo, item.span().lo())), has_comments)
            } else {
                (None, false)
            };
            let prev_has_trailing_comment = match prev_trailing_span {
                Some(ts) => contains_comment(context.snippet(ts)),
                _ => false,
            };

            let shape = if need_indent && force_newline {
                shape
                    .block_indent(context.config.tab_spaces())
                    .with_max_width(context.config)
            } else {
                shape
            };
            let whitespace = if force_newline && (!prev_extendable || !generic_bounds_in_order) {
                shape
                    .indent
                    .to_string_with_newline(context.config)
                    .to_string()
            } else {
                String::from(" ")
            };

            let joiner = match context.config.type_punctuation_density() {
                TypeDensity::Compressed => String::from("+"),
                TypeDensity::Wide => whitespace + "+ ",
            };
            let joiner = if has_leading_comment {
                joiner.trim_end()
            } else {
                &joiner
            };
            let joiner = if prev_has_trailing_comment {
                joiner.trim_start()
            } else {
                joiner
            };

            let (extendable, trailing_str) = if i == 0 {
                let bound_str = item.rewrite(context, shape)?;
                (is_bound_extendable(&bound_str, item), bound_str)
            } else {
                let bound_str = &item.rewrite(context, shape)?;
                match leading_span {
                    Some(ls) if has_leading_comment => (
                        is_bound_extendable(bound_str, item),
                        combine_strs_with_missing_comments(
                            context, joiner, bound_str, ls, shape, true,
                        )?,
                    ),
                    _ => (
                        is_bound_extendable(bound_str, item),
                        String::from(joiner) + bound_str,
                    ),
                }
            };
            match prev_trailing_span {
                Some(ts) if prev_has_trailing_comment => combine_strs_with_missing_comments(
                    context,
                    &strs,
                    &trailing_str,
                    ts,
                    shape,
                    true,
                )
                .map(|v| (v, trailing_span, extendable)),
                _ => Some((strs + &trailing_str, trailing_span, extendable)),
            }
        },
    )?;

    // Whether retry the function with forced newline is needed:
    //   Only if result is not already multiline and did not exceed line width,
    //   and either there is more than one item;
    //       or the single item is of type `Trait`,
    //          and any of the internal arrays contains more than one item;
    let retry_with_force_newline =
        if force_newline || (!result.0.contains('\n') && result.0.len() <= shape.width) {
            false
        } else {
            if items.len() > 1 {
                true
            } else {
                match items[0] {
                    ast::GenericBound::Trait(ref poly_trait_ref, ..) => {
                        let segments = &poly_trait_ref.trait_ref.path.segments;
                        if segments.len() > 1 {
                            true
                        } else {
                            is_segment_with_multi_items_array(&segments[0])
                        }
                    }
                    _ => false,
                }
            }
        };

    if retry_with_force_newline {
        join_bounds_inner(context, shape, items, need_indent, true)
    } else {
        Some(result.0)
    }
}

pub(crate) fn can_be_overflowed_type(
    context: &RewriteContext<'_>,
    ty: &ast::Ty,
    len: usize,
) -> bool {
    match ty.kind {
        ast::TyKind::Tup(..) => context.use_block_indent() && len == 1,
        ast::TyKind::Rptr(_, ref mutty) | ast::TyKind::Ptr(ref mutty) => {
            can_be_overflowed_type(context, &*mutty.ty, len)
        }
        _ => false,
    }
}

/// Returns rewritten params separated by commas, or `None` if there are no params or a param
/// failed to be rewritten.
fn rewrite_generic_params(
    context: &RewriteContext<'_>,
    shape: Shape,
    generic_params: &[ast::GenericParam],
) -> Option<String> {
    if generic_params.is_empty() {
        return None;
    }
    let result = generic_params
        .iter()
        .map(|lt| lt.rewrite(context, shape))
        .collect::<Option<Vec<_>>>()?
        .join(", ");
    Some(result)
}

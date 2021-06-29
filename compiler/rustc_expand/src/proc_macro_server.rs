use crate::base::ExtCtxt;

use rustc_ast as ast;
use rustc_ast::token;
use rustc_ast::tokenstream::{self, DelimSpan, Spacing::*, TokenStream, TreeAndSpacing};
use rustc_ast_pretty::pprust;
use rustc_data_structures::fx::FxHashMap;
use rustc_data_structures::sync::Lrc;
use rustc_errors::{Diagnostic, MultiSpan, PResult};
use rustc_parse::lexer::nfc_normalize;
use rustc_parse::parse_stream_from_source_str;
use rustc_session::parse::ParseSess;
use rustc_span::def_id::CrateNum;
use rustc_span::symbol::{self, kw, sym, Symbol};
use rustc_span::{BytePos, FileName, Pos, SourceFile, Span};

use pm::bridge::{server, ExpnGlobals, Punct, TokenTree};
use pm::{Delimiter, Level, LineColumn};
use std::ops::Bound;
use std::{ascii, panic};

trait FromInternal<T> {
    fn from_internal(x: T) -> Self;
}

trait ToInternal<T> {
    fn to_internal(self) -> T;
}

impl FromInternal<token::Delimiter> for Delimiter {
    fn from_internal(delim: token::Delimiter) -> Delimiter {
        match delim {
            token::Delimiter::Parenthesis => Delimiter::Parenthesis,
            token::Delimiter::Brace => Delimiter::Brace,
            token::Delimiter::Bracket => Delimiter::Bracket,
            token::Delimiter::Invisible => Delimiter::None,
        }
    }
}

impl ToInternal<token::Delimiter> for Delimiter {
    fn to_internal(self) -> token::Delimiter {
        match self {
            Delimiter::Parenthesis => token::Delimiter::Parenthesis,
            Delimiter::Brace => token::Delimiter::Brace,
            Delimiter::Bracket => token::Delimiter::Bracket,
            Delimiter::None => token::Delimiter::Invisible,
        }
    }
}

impl FromInternal<(TreeAndSpacing, &'_ mut Vec<Self>, &mut Rustc<'_, '_>)>
    for TokenTree<Span, Group, Ident, Literal>
{
    fn from_internal(
        ((tree, spacing), stack, rustc): (TreeAndSpacing, &mut Vec<Self>, &mut Rustc<'_, '_>),
    ) -> Self {
        use rustc_ast::token::*;

        let joint = spacing == Joint;
        let Token { kind, span } = match tree {
            tokenstream::TokenTree::Delimited(span, delim, tts) => {
                let delimiter = pm::Delimiter::from_internal(delim);
                return TokenTree::Group(Group { delimiter, stream: tts, span, flatten: false });
            }
            tokenstream::TokenTree::Token(token) => token,
        };

        macro_rules! tt {
            ($ty:ident { $($field:ident $(: $value:expr)*),+ $(,)? }) => (
                TokenTree::$ty(self::$ty {
                    $($field $(: $value)*,)+
                    span,
                })
            );
            ($ty:ident::$method:ident($($value:expr),*)) => (
                TokenTree::$ty(self::$ty::$method($($value,)* span))
            );
        }
        macro_rules! op {
            ($a:expr) => {
                tt!(Punct { ch: $a, joint })
            };
            ($a:expr, $b:expr) => {{
                stack.push(tt!(Punct { ch: $b, joint }));
                tt!(Punct { ch: $a, joint: true })
            }};
            ($a:expr, $b:expr, $c:expr) => {{
                stack.push(tt!(Punct { ch: $c, joint }));
                stack.push(tt!(Punct { ch: $b, joint: true }));
                tt!(Punct { ch: $a, joint: true })
            }};
        }

        match kind {
            Eq => op!('='),
            Lt => op!('<'),
            Le => op!('<', '='),
            EqEq => op!('=', '='),
            Ne => op!('!', '='),
            Ge => op!('>', '='),
            Gt => op!('>'),
            AndAnd => op!('&', '&'),
            OrOr => op!('|', '|'),
            Not => op!('!'),
            Tilde => op!('~'),
            BinOp(Plus) => op!('+'),
            BinOp(Minus) => op!('-'),
            BinOp(Star) => op!('*'),
            BinOp(Slash) => op!('/'),
            BinOp(Percent) => op!('%'),
            BinOp(Caret) => op!('^'),
            BinOp(And) => op!('&'),
            BinOp(Or) => op!('|'),
            BinOp(Shl) => op!('<', '<'),
            BinOp(Shr) => op!('>', '>'),
            BinOpEq(Plus) => op!('+', '='),
            BinOpEq(Minus) => op!('-', '='),
            BinOpEq(Star) => op!('*', '='),
            BinOpEq(Slash) => op!('/', '='),
            BinOpEq(Percent) => op!('%', '='),
            BinOpEq(Caret) => op!('^', '='),
            BinOpEq(And) => op!('&', '='),
            BinOpEq(Or) => op!('|', '='),
            BinOpEq(Shl) => op!('<', '<', '='),
            BinOpEq(Shr) => op!('>', '>', '='),
            At => op!('@'),
            Dot => op!('.'),
            DotDot => op!('.', '.'),
            DotDotDot => op!('.', '.', '.'),
            DotDotEq => op!('.', '.', '='),
            Comma => op!(','),
            Semi => op!(';'),
            Colon => op!(':'),
            ModSep => op!(':', ':'),
            RArrow => op!('-', '>'),
            LArrow => op!('<', '-'),
            FatArrow => op!('=', '>'),
            Pound => op!('#'),
            Dollar => op!('$'),
            Question => op!('?'),
            SingleQuote => op!('\''),

            Ident(name, false) if name == kw::DollarCrate => tt!(Ident::dollar_crate()),
            Ident(name, is_raw) => tt!(Ident::new(rustc.sess(), name, is_raw)),
            Lifetime(name) => {
                let ident = symbol::Ident::new(name, span).without_first_quote();
                stack.push(tt!(Ident::new(rustc.sess(), ident.name, false)));
                tt!(Punct { ch: '\'', joint: true })
            }
            Literal(lit) => tt!(Literal { lit }),
            DocComment(_, attr_style, data) => {
                let mut escaped = String::new();
                for ch in data.as_str().chars() {
                    escaped.extend(ch.escape_debug());
                }
                let stream = [
                    Ident(sym::doc, false),
                    Eq,
                    TokenKind::lit(token::Str, Symbol::intern(&escaped), None),
                ]
                .into_iter()
                .map(|kind| tokenstream::TokenTree::token(kind, span))
                .collect();
                stack.push(TokenTree::Group(Group {
                    delimiter: pm::Delimiter::Bracket,
                    stream,
                    span: DelimSpan::from_single(span),
                    flatten: false,
                }));
                if attr_style == ast::AttrStyle::Inner {
                    stack.push(tt!(Punct { ch: '!', joint: false }));
                }
                tt!(Punct { ch: '#', joint: false })
            }

            Interpolated(nt) if let NtIdent(ident, is_raw) = *nt => {
                TokenTree::Ident(Ident::new(rustc.sess(), ident.name, is_raw, ident.span))
            }
            Interpolated(nt) => {
                TokenTree::Group(Group {
                    delimiter: pm::Delimiter::None,
                    stream: TokenStream::from_nonterminal_ast(&nt),
                    span: DelimSpan::from_single(span),
                    flatten: crate::base::nt_pretty_printing_compatibility_hack(&nt, rustc.sess()),
                })
            }

            OpenDelim(..) | CloseDelim(..) => unreachable!(),
            Eof => unreachable!(),
        }
    }
}

impl ToInternal<TokenStream> for TokenTree<Span, Group, Ident, Literal> {
    fn to_internal(self) -> TokenStream {
        use rustc_ast::token::*;

        let (ch, joint, span) = match self {
            TokenTree::Punct(Punct { ch, joint, span }) => (ch, joint, span),
            TokenTree::Group(Group { delimiter, stream, span, .. }) => {
                return tokenstream::TokenTree::Delimited(span, delimiter.to_internal(), stream)
                    .into();
            }
            TokenTree::Ident(self::Ident { sym, is_raw, span }) => {
                return tokenstream::TokenTree::token(Ident(sym, is_raw), span).into();
            }
            TokenTree::Literal(self::Literal {
                lit: token::Lit { kind: token::Integer, symbol, suffix },
                span,
            }) if symbol.as_str().starts_with('-') => {
                let minus = BinOp(BinOpToken::Minus);
                let symbol = Symbol::intern(&symbol.as_str()[1..]);
                let integer = TokenKind::lit(token::Integer, symbol, suffix);
                let a = tokenstream::TokenTree::token(minus, span);
                let b = tokenstream::TokenTree::token(integer, span);
                return [a, b].into_iter().collect();
            }
            TokenTree::Literal(self::Literal {
                lit: token::Lit { kind: token::Float, symbol, suffix },
                span,
            }) if symbol.as_str().starts_with('-') => {
                let minus = BinOp(BinOpToken::Minus);
                let symbol = Symbol::intern(&symbol.as_str()[1..]);
                let float = TokenKind::lit(token::Float, symbol, suffix);
                let a = tokenstream::TokenTree::token(minus, span);
                let b = tokenstream::TokenTree::token(float, span);
                return [a, b].into_iter().collect();
            }
            TokenTree::Literal(self::Literal { lit, span }) => {
                return tokenstream::TokenTree::token(Literal(lit), span).into();
            }
        };

        let kind = match ch {
            '=' => Eq,
            '<' => Lt,
            '>' => Gt,
            '!' => Not,
            '~' => Tilde,
            '+' => BinOp(Plus),
            '-' => BinOp(Minus),
            '*' => BinOp(Star),
            '/' => BinOp(Slash),
            '%' => BinOp(Percent),
            '^' => BinOp(Caret),
            '&' => BinOp(And),
            '|' => BinOp(Or),
            '@' => At,
            '.' => Dot,
            ',' => Comma,
            ';' => Semi,
            ':' => Colon,
            '#' => Pound,
            '$' => Dollar,
            '?' => Question,
            '\'' => SingleQuote,
            _ => unreachable!(),
        };

        let tree = tokenstream::TokenTree::token(kind, span);
        TokenStream::new(vec![(tree, if joint { Joint } else { Alone })])
    }
}

impl ToInternal<rustc_errors::Level> for Level {
    fn to_internal(self) -> rustc_errors::Level {
        match self {
            Level::Error => rustc_errors::Level::Error { lint: false },
            Level::Warning => rustc_errors::Level::Warning(None),
            Level::Note => rustc_errors::Level::Note,
            Level::Help => rustc_errors::Level::Help,
            _ => unreachable!("unknown proc_macro::Level variant: {:?}", self),
        }
    }
}

pub struct FreeFunctions;

#[derive(Clone)]
pub struct Group {
    delimiter: Delimiter,
    stream: TokenStream,
    span: DelimSpan,
    /// A hack used to pass AST fragments to attribute and derive macros
    /// as a single nonterminal token instead of a token stream.
    /// FIXME: It needs to be removed, but there are some compatibility issues (see #73345).
    flatten: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct Ident {
    sym: Symbol,
    is_raw: bool,
    span: Span,
}

impl Ident {
    fn new(sess: &ParseSess, sym: Symbol, is_raw: bool, span: Span) -> Ident {
        let sym = nfc_normalize(sym.as_str());
        let string = sym.as_str();
        if !rustc_lexer::is_ident(string) {
            panic!("`{:?}` is not a valid identifier", string)
        }
        if is_raw && !sym.can_be_raw() {
            panic!("`{}` cannot be a raw identifier", string);
        }
        sess.symbol_gallery.insert(sym, span);
        Ident { sym, is_raw, span }
    }

    fn dollar_crate(span: Span) -> Ident {
        // `$crate` is accepted as an ident only if it comes from the compiler.
        Ident { sym: kw::DollarCrate, is_raw: false, span }
    }
}

// FIXME(eddyb) `Literal` should not expose internal `Debug` impls.
#[derive(Clone, Debug)]
pub struct Literal {
    lit: token::Lit,
    span: Span,
}

pub(crate) struct Rustc<'a, 'b> {
    ecx: &'a mut ExtCtxt<'b>,
    def_site: Span,
    call_site: Span,
    mixed_site: Span,
    krate: CrateNum,
    rebased_spans: FxHashMap<usize, Span>,
}

impl<'a, 'b> Rustc<'a, 'b> {
    pub fn new(ecx: &'a mut ExtCtxt<'b>) -> Self {
        let expn_data = ecx.current_expansion.id.expn_data();
        Rustc {
            def_site: ecx.with_def_site_ctxt(expn_data.def_site),
            call_site: ecx.with_call_site_ctxt(expn_data.call_site),
            mixed_site: ecx.with_mixed_site_ctxt(expn_data.call_site),
            krate: expn_data.macro_def_id.unwrap().krate,
            rebased_spans: FxHashMap::default(),
            ecx,
        }
    }

    fn sess(&self) -> &ParseSess {
        self.ecx.parse_sess()
    }

    fn lit(&mut self, kind: token::LitKind, symbol: Symbol, suffix: Option<Symbol>) -> Literal {
        Literal { lit: token::Lit::new(kind, symbol, suffix), span: self.call_site }
    }
}

impl server::Types for Rustc<'_, '_> {
    type FreeFunctions = FreeFunctions;
    type TokenStream = TokenStream;
    type Group = Group;
    type Ident = Ident;
    type Literal = Literal;
    type SourceFile = Lrc<SourceFile>;
    type MultiSpan = Vec<Span>;
    type Diagnostic = Diagnostic;
    type Span = Span;
}

impl server::FreeFunctions for Rustc<'_, '_> {
    fn track_env_var(&mut self, var: &str, value: Option<&str>) {
        self.sess()
            .env_depinfo
            .borrow_mut()
            .insert((Symbol::intern(var), value.map(Symbol::intern)));
    }

    fn track_path(&mut self, path: &str) {
        self.sess().file_depinfo.borrow_mut().insert(Symbol::intern(path));
    }
}

impl server::TokenStream for Rustc<'_, '_> {
    fn is_empty(&mut self, stream: &Self::TokenStream) -> bool {
        stream.is_empty()
    }

    fn from_str(&mut self, src: &str) -> Self::TokenStream {
        parse_stream_from_source_str(
            FileName::proc_macro_source_code(src),
            src.to_string(),
            self.sess(),
            Some(self.call_site),
        )
    }

    fn to_string(&mut self, stream: &Self::TokenStream) -> String {
        pprust::tts_to_string(stream)
    }

    fn expand_expr(&mut self, stream: &Self::TokenStream) -> Result<Self::TokenStream, ()> {
        // Parse the expression from our tokenstream.
        let expr: PResult<'_, _> = try {
            let mut p = rustc_parse::stream_to_parser(
                self.sess(),
                stream.clone(),
                Some("proc_macro expand expr"),
            );
            let expr = p.parse_expr()?;
            if p.token != token::Eof {
                p.unexpected()?;
            }
            expr
        };
        let expr = expr.map_err(|mut err| {
            err.emit();
        })?;

        // Perform eager expansion on the expression.
        let expr = self
            .ecx
            .expander()
            .fully_expand_fragment(crate::expand::AstFragment::Expr(expr))
            .make_expr();

        // NOTE: For now, limit `expand_expr` to exclusively expand to literals.
        // This may be relaxed in the future.
        // We don't use `TokenStream::from_ast` as the tokenstream currently cannot
        // be recovered in the general case.
        match &expr.kind {
            ast::ExprKind::Lit(l) => {
                Ok(tokenstream::TokenTree::token(token::Literal(l.token), l.span).into())
            }
            ast::ExprKind::Unary(ast::UnOp::Neg, e) => match &e.kind {
                ast::ExprKind::Lit(l) => match l.token {
                    token::Lit { kind: token::Integer | token::Float, .. } => {
                        Ok(Self::TokenStream::from_iter([
                            // FIXME: The span of the `-` token is lost when
                            // parsing, so we cannot faithfully recover it here.
                            tokenstream::TokenTree::token(token::BinOp(token::Minus), e.span),
                            tokenstream::TokenTree::token(token::Literal(l.token), l.span),
                        ]))
                    }
                    _ => Err(()),
                },
                _ => Err(()),
            },
            _ => Err(()),
        }
    }

    fn from_token_tree(
        &mut self,
        tree: TokenTree<Self::Span, Self::Group, Self::Ident, Self::Literal>,
    ) -> Self::TokenStream {
        tree.to_internal()
    }

    fn concat_trees(
        &mut self,
        base: Option<Self::TokenStream>,
        trees: Vec<TokenTree<Self::Span, Self::Group, Self::Ident, Self::Literal>>,
    ) -> Self::TokenStream {
        let mut builder = tokenstream::TokenStreamBuilder::new();
        if let Some(base) = base {
            builder.push(base);
        }
        for tree in trees {
            builder.push(tree.to_internal());
        }
        builder.build()
    }

    fn concat_streams(
        &mut self,
        base: Option<Self::TokenStream>,
        streams: Vec<Self::TokenStream>,
    ) -> Self::TokenStream {
        let mut builder = tokenstream::TokenStreamBuilder::new();
        if let Some(base) = base {
            builder.push(base);
        }
        for stream in streams {
            builder.push(stream);
        }
        builder.build()
    }

    fn into_trees(
        &mut self,
        stream: Self::TokenStream,
    ) -> Vec<TokenTree<Self::Span, Self::Group, Self::Ident, Self::Literal>> {
        // FIXME: This is a raw port of the previous approach (which had a
        // `TokenStreamIter` server-side object with a single `next` method),
        // and can probably be optimized (for bulk conversion).
        let mut cursor = stream.into_trees();
        let mut stack = Vec::new();
        let mut tts = Vec::new();
        loop {
            let next = stack.pop().or_else(|| {
                let next = cursor.next_with_spacing()?;
                Some(TokenTree::from_internal((next, &mut stack, self)))
            });
            match next {
                Some(TokenTree::Group(group)) => {
                    // A hack used to pass AST fragments to attribute and derive
                    // macros as a single nonterminal token instead of a token
                    // stream.  Such token needs to be "unwrapped" and not
                    // represented as a delimited group.
                    // FIXME: It needs to be removed, but there are some
                    // compatibility issues (see #73345).
                    if group.flatten {
                        tts.append(&mut self.into_trees(group.stream));
                    } else {
                        tts.push(TokenTree::Group(group));
                    }
                }
                Some(tt) => tts.push(tt),
                None => return tts,
            }
        }
    }
}

impl server::Group for Rustc<'_, '_> {
    fn new(&mut self, delimiter: Delimiter, stream: Option<Self::TokenStream>) -> Self::Group {
        Group {
            delimiter,
            stream: stream.unwrap_or_default(),
            span: DelimSpan::from_single(self.call_site),
            flatten: false,
        }
    }

    fn delimiter(&mut self, group: &Self::Group) -> Delimiter {
        group.delimiter
    }

    fn stream(&mut self, group: &Self::Group) -> Self::TokenStream {
        group.stream.clone()
    }

    fn span(&mut self, group: &Self::Group) -> Self::Span {
        group.span.entire()
    }

    fn span_open(&mut self, group: &Self::Group) -> Self::Span {
        group.span.open
    }

    fn span_close(&mut self, group: &Self::Group) -> Self::Span {
        group.span.close
    }

    fn set_span(&mut self, group: &mut Self::Group, span: Self::Span) {
        group.span = DelimSpan::from_single(span);
    }
}

impl server::Ident for Rustc<'_, '_> {
    fn new(&mut self, string: &str, span: Self::Span, is_raw: bool) -> Self::Ident {
        Ident::new(self.sess(), Symbol::intern(string), is_raw, span)
    }

    fn span(&mut self, ident: Self::Ident) -> Self::Span {
        ident.span
    }

    fn with_span(&mut self, ident: Self::Ident, span: Self::Span) -> Self::Ident {
        Ident { span, ..ident }
    }
}

impl server::Literal for Rustc<'_, '_> {
    fn from_str(&mut self, s: &str) -> Result<Self::Literal, ()> {
        let name = FileName::proc_macro_source_code(s);
        let mut parser = rustc_parse::new_parser_from_source_str(self.sess(), name, s.to_owned());

        let first_span = parser.token.span.data();
        let minus_present = parser.eat(&token::BinOp(token::Minus));

        let lit_span = parser.token.span.data();
        let token::Literal(mut lit) = parser.token.kind else {
            return Err(());
        };

        // Check no comment or whitespace surrounding the (possibly negative)
        // literal, or more tokens after it.
        if (lit_span.hi.0 - first_span.lo.0) as usize != s.len() {
            return Err(());
        }

        if minus_present {
            // If minus is present, check no comment or whitespace in between it
            // and the literal token.
            if first_span.hi.0 != lit_span.lo.0 {
                return Err(());
            }

            // Check literal is a kind we allow to be negated in a proc macro token.
            match lit.kind {
                token::LitKind::Bool
                | token::LitKind::Byte
                | token::LitKind::Char
                | token::LitKind::Str
                | token::LitKind::StrRaw(_)
                | token::LitKind::ByteStr
                | token::LitKind::ByteStrRaw(_)
                | token::LitKind::Err => return Err(()),
                token::LitKind::Integer | token::LitKind::Float => {}
            }

            // Synthesize a new symbol that includes the minus sign.
            let symbol = Symbol::intern(&s[..1 + lit.symbol.as_str().len()]);
            lit = token::Lit::new(lit.kind, symbol, lit.suffix);
        }

        Ok(Literal { lit, span: self.call_site })
    }

    fn to_string(&mut self, literal: &Self::Literal) -> String {
        literal.lit.to_string()
    }

    fn debug_kind(&mut self, literal: &Self::Literal) -> String {
        format!("{:?}", literal.lit.kind)
    }

    fn symbol(&mut self, literal: &Self::Literal) -> String {
        literal.lit.symbol.to_string()
    }

    fn suffix(&mut self, literal: &Self::Literal) -> Option<String> {
        literal.lit.suffix.as_ref().map(Symbol::to_string)
    }

    fn integer(&mut self, n: &str) -> Self::Literal {
        self.lit(token::Integer, Symbol::intern(n), None)
    }

    fn typed_integer(&mut self, n: &str, kind: &str) -> Self::Literal {
        self.lit(token::Integer, Symbol::intern(n), Some(Symbol::intern(kind)))
    }

    fn float(&mut self, n: &str) -> Self::Literal {
        self.lit(token::Float, Symbol::intern(n), None)
    }

    fn f32(&mut self, n: &str) -> Self::Literal {
        self.lit(token::Float, Symbol::intern(n), Some(sym::f32))
    }

    fn f64(&mut self, n: &str) -> Self::Literal {
        self.lit(token::Float, Symbol::intern(n), Some(sym::f64))
    }

    fn string(&mut self, string: &str) -> Self::Literal {
        let quoted = format!("{:?}", string);
        assert!(quoted.starts_with('"') && quoted.ends_with('"'));
        let symbol = &quoted[1..quoted.len() - 1];
        self.lit(token::Str, Symbol::intern(symbol), None)
    }

    fn character(&mut self, ch: char) -> Self::Literal {
        let quoted = format!("{:?}", ch);
        assert!(quoted.starts_with('\'') && quoted.ends_with('\''));
        let symbol = &quoted[1..quoted.len() - 1];
        self.lit(token::Char, Symbol::intern(symbol), None)
    }

    fn byte_string(&mut self, bytes: &[u8]) -> Self::Literal {
        let string = bytes
            .iter()
            .cloned()
            .flat_map(ascii::escape_default)
            .map(Into::<char>::into)
            .collect::<String>();
        self.lit(token::ByteStr, Symbol::intern(&string), None)
    }

    fn span(&mut self, literal: &Self::Literal) -> Self::Span {
        literal.span
    }

    fn set_span(&mut self, literal: &mut Self::Literal, span: Self::Span) {
        literal.span = span;
    }

    fn subspan(
        &mut self,
        literal: &Self::Literal,
        start: Bound<usize>,
        end: Bound<usize>,
    ) -> Option<Self::Span> {
        let span = literal.span;
        let length = span.hi().to_usize() - span.lo().to_usize();

        let start = match start {
            Bound::Included(lo) => lo,
            Bound::Excluded(lo) => lo.checked_add(1)?,
            Bound::Unbounded => 0,
        };

        let end = match end {
            Bound::Included(hi) => hi.checked_add(1)?,
            Bound::Excluded(hi) => hi,
            Bound::Unbounded => length,
        };

        // Bounds check the values, preventing addition overflow and OOB spans.
        if start > u32::MAX as usize
            || end > u32::MAX as usize
            || (u32::MAX - start as u32) < span.lo().to_u32()
            || (u32::MAX - end as u32) < span.lo().to_u32()
            || start >= end
            || end > length
        {
            return None;
        }

        let new_lo = span.lo() + BytePos::from_usize(start);
        let new_hi = span.lo() + BytePos::from_usize(end);
        Some(span.with_lo(new_lo).with_hi(new_hi))
    }
}

impl server::SourceFile for Rustc<'_, '_> {
    fn eq(&mut self, file1: &Self::SourceFile, file2: &Self::SourceFile) -> bool {
        Lrc::ptr_eq(file1, file2)
    }

    fn path(&mut self, file: &Self::SourceFile) -> String {
        match file.name {
            FileName::Real(ref name) => name
                .local_path()
                .expect("attempting to get a file path in an imported file in `proc_macro::SourceFile::path`")
                .to_str()
                .expect("non-UTF8 file path in `proc_macro::SourceFile::path`")
                .to_string(),
            _ => file.name.prefer_local().to_string(),
        }
    }

    fn is_real(&mut self, file: &Self::SourceFile) -> bool {
        file.is_real_file()
    }
}

impl server::MultiSpan for Rustc<'_, '_> {
    fn new(&mut self) -> Self::MultiSpan {
        vec![]
    }

    fn push(&mut self, spans: &mut Self::MultiSpan, span: Self::Span) {
        spans.push(span)
    }
}

impl server::Diagnostic for Rustc<'_, '_> {
    fn new(&mut self, level: Level, msg: &str, spans: Self::MultiSpan) -> Self::Diagnostic {
        let mut diag = Diagnostic::new(level.to_internal(), msg);
        diag.set_span(MultiSpan::from_spans(spans));
        diag
    }

    fn sub(
        &mut self,
        diag: &mut Self::Diagnostic,
        level: Level,
        msg: &str,
        spans: Self::MultiSpan,
    ) {
        diag.sub(level.to_internal(), msg, MultiSpan::from_spans(spans), None);
    }

    fn emit(&mut self, mut diag: Self::Diagnostic) {
        self.sess().span_diagnostic.emit_diagnostic(&mut diag);
    }
}

impl server::Span for Rustc<'_, '_> {
    fn debug(&mut self, span: Self::Span) -> String {
        if self.ecx.ecfg.span_debug {
            format!("{:?}", span)
        } else {
            format!("{:?} bytes({}..{})", span.ctxt(), span.lo().0, span.hi().0)
        }
    }

    fn source_file(&mut self, span: Self::Span) -> Self::SourceFile {
        self.sess().source_map().lookup_char_pos(span.lo()).file
    }

    fn parent(&mut self, span: Self::Span) -> Option<Self::Span> {
        span.parent_callsite()
    }

    fn source(&mut self, span: Self::Span) -> Self::Span {
        span.source_callsite()
    }

    fn start(&mut self, span: Self::Span) -> LineColumn {
        let loc = self.sess().source_map().lookup_char_pos(span.lo());
        LineColumn { line: loc.line, column: loc.col.to_usize() }
    }

    fn end(&mut self, span: Self::Span) -> LineColumn {
        let loc = self.sess().source_map().lookup_char_pos(span.hi());
        LineColumn { line: loc.line, column: loc.col.to_usize() }
    }

    fn before(&mut self, span: Self::Span) -> Self::Span {
        span.shrink_to_lo()
    }

    fn after(&mut self, span: Self::Span) -> Self::Span {
        span.shrink_to_hi()
    }

    fn join(&mut self, first: Self::Span, second: Self::Span) -> Option<Self::Span> {
        let self_loc = self.sess().source_map().lookup_char_pos(first.lo());
        let other_loc = self.sess().source_map().lookup_char_pos(second.lo());

        if self_loc.file.name != other_loc.file.name {
            return None;
        }

        Some(first.to(second))
    }

    fn resolved_at(&mut self, span: Self::Span, at: Self::Span) -> Self::Span {
        span.with_ctxt(at.ctxt())
    }

    fn source_text(&mut self, span: Self::Span) -> Option<String> {
        self.sess().source_map().span_to_snippet(span).ok()
    }
    /// Saves the provided span into the metadata of
    /// *the crate we are currently compiling*, which must
    /// be a proc-macro crate. This id can be passed to
    /// `recover_proc_macro_span` when our current crate
    /// is *run* as a proc-macro.
    ///
    /// Let's suppose that we have two crates - `my_client`
    /// and `my_proc_macro`. The `my_proc_macro` crate
    /// contains a procedural macro `my_macro`, which
    /// is implemented as: `quote! { "hello" }`
    ///
    /// When we *compile* `my_proc_macro`, we will execute
    /// the `quote` proc-macro. This will save the span of
    /// "hello" into the metadata of `my_proc_macro`. As a result,
    /// the body of `my_proc_macro` (after expansion) will end
    /// up containing a call that looks like this:
    /// `proc_macro::Ident::new("hello", proc_macro::Span::recover_proc_macro_span(0))`
    ///
    /// where `0` is the id returned by this function.
    /// When `my_proc_macro` *executes* (during the compilation of `my_client`),
    /// the call to `recover_proc_macro_span` will load the corresponding
    /// span from the metadata of `my_proc_macro` (which we have access to,
    /// since we've loaded `my_proc_macro` from disk in order to execute it).
    /// In this way, we have obtained a span pointing into `my_proc_macro`
    fn save_span(&mut self, span: Self::Span) -> usize {
        self.sess().save_proc_macro_span(span)
    }

    fn recover_proc_macro_span(&mut self, id: usize) -> Self::Span {
        let (resolver, krate, def_site) = (&*self.ecx.resolver, self.krate, self.def_site);
        *self.rebased_spans.entry(id).or_insert_with(|| {
            // FIXME: `SyntaxContext` for spans from proc macro crates is lost during encoding,
            // replace it with a def-site context until we are encoding it properly.
            resolver.get_proc_macro_quoted_span(krate, id).with_ctxt(def_site.ctxt())
        })
    }
}

impl server::Server for Rustc<'_, '_> {
    fn globals(&mut self) -> ExpnGlobals<Self::Span> {
        ExpnGlobals {
            def_site: self.def_site,
            call_site: self.call_site,
            mixed_site: self.mixed_site,
        }
    }
}

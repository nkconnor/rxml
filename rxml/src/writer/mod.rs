/*!
# Writer for restricted XML 1.0
*/
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::HashSet;
use std::convert::TryInto;
use std::fmt;

use bytes::{BufMut, BytesMut};

use crate::parser::{NamespaceName, RcPtr, ResolvedEvent, XmlVersion, XMLNS_XML, XMLNS_XMLNS};
use crate::strings::{CData, CDataStr, Name, NcName, NcNameStr};

const XML_DECL: &'static [u8] = b"<?xml version='1.0' encoding='utf-8'?>\n";
pub const PREFIX_XML: &'static NcNameStr = unsafe { std::mem::transmute("xml") };
pub const PREFIX_XMLNS: &'static NcNameStr = unsafe { std::mem::transmute("xmlns") };

const CDATA_SPECIALS: &'static [u8] = &[b'<', b'>', b'&', b'\r'];

const ATTR_SPECIALS: &'static [u8] = &[b'"', b'\'', b'\r', b'\n', b'\t', b'<', b'>', b'&'];

const CDATAOPEN: &'static str = "<![CDATA[";
const CDATACLOSE: &'static str = "]]>";

fn escape<'a, B: BufMut>(out: &'a mut B, data: &'a [u8], specials: &'static [u8]) {
	let mut last_index = 0;
	for i in 0..data.len() {
		let ch = data[i];
		if !specials.contains(&ch) {
			continue;
		}
		if i > last_index {
			out.put_slice(&data[last_index..i]);
		}
		match ch {
			b'"' => out.put_slice(b"&#34;"),
			b'\'' => out.put_slice(b"&#39;"),
			b'<' => out.put_slice(b"&lt;"),
			b'>' => out.put_slice(b"&gt;"),
			b'&' => out.put_slice(b"&amp;"),
			b'\r' => out.put_slice(b"&#xd;"),
			b'\n' => out.put_slice(b"&#xa;"),
			b'\t' => out.put_slice(b"&#x9;"),
			_ => panic!("unexpected special character?!"),
		}
		last_index = i + 1;
	}
	out.put_slice(&data[last_index..data.len()]);
}

/// An encodable item.
///
/// This is separate from [`ResolvedEvent`], because events are owned, while
/// items can be borrowed to improve efficiency (as a copy will have to take
/// place anyway).
///
///   [`ResolvedEvent`]: crate::parser::ResolvedEvent
pub enum Item<'x> {
	/// XML declaration
	XmlDeclaration(XmlVersion),

	/// Start of an element header
	ElementHeadStart(
		/// Namespace URI or None, for unnamespaced elements
		Option<RcPtr<CData>>,
		/// Local name of the attribute
		&'x NcNameStr,
	),

	/// An attribute key/value pair
	Attribute(
		/// Namespace URI or None, for unnamespaced attributes
		Option<RcPtr<CData>>,
		/// Local name of the attribute
		&'x NcNameStr,
		/// Value of the attribute
		&'x CDataStr,
	),

	/// End of an element header
	ElementHeadEnd,

	/// A piece of text (in element content, not attributes)
	Text(&'x CDataStr),

	/// A raw, unescaped piece of text (in element content, not attributes)
	RawText(&'x str),

	/// CDATA start <![CDATA[
	CDataOpen,

	/// CDATA close ]]>
	CDataClose,

	/// Footer of an element
	///
	/// This can be used either in places where [`Text`] could be used to
	/// close the most recently opened unclosed element, or it can be used
	/// instead of [`ElementHeadEnd`] to close the element using `/>`, without
	/// any child content.
	///
	///   [`Text`]: Self::Text
	///   [`ElementHeadEnd`]: Self::ElementHeadEnd
	ElementFoot,
}

#[derive(Debug)]
pub enum PrefixError {
	Undeclared,
}

/// Trait for a thing tracking namespace declarations.
///
/// Indirection via this trait allows to have different paradigms for
/// declaring and managing namespace prefixes.
///
/// Objects implementing this trait expect the following protocol:
///
/// 1. Declare all namespace URIs introduced on an element using `declare` and `declare_auto`.
/// 2. Commit to the element using `push`
/// 3. Process all child elements by recursion
/// 4. Call `pop` to end the element.
///
/// Asymmetric calls to push/pop may cause panics or memory leaks.
pub trait TrackNamespace {
	/// Declare a namespace URI with a defined prefix.
	///
	/// Note: There is no guarantee that the given `prefix` will be returned
	/// from calls to `get_prefix` or `get_prefix_or_default` after the next
	/// call to `push`.
	///
	/// Returns whether the prefix is freshly declared.
	///
	/// # Panics
	///
	/// Calling this twice between two calls to `push` with the same `prefix`
	/// is a programming error and causes a panic.
	fn declare_fixed(&mut self, prefix: Option<&NcNameStr>, name: Option<NamespaceName>) -> bool;

	/// Declare a namespace URI with an auto-generated prefix or by using the
	/// default namespace.
	///
	/// Note: There is no guarantee that the returned `prefix` will be
	/// returned from calls to `get_prefix_or_default` after the next call to
	/// `push`.
	///
	/// Returns whether the prefix is freshly declared and the
	/// resulting prefix (or None, if prefixless).
	///
	/// This may return a non-auto-generated prefix if the namespace URI is
	/// already declared on this or a parent element.
	fn declare_auto(&mut self, name: Option<NamespaceName>) -> (bool, Option<&NcNameStr>);

	/// Declare a namespace URI with an auto-generated prefix.
	///
	/// Note: There is no guarantee that the returned `prefix` will be
	/// returned from calls to `get_prefix` or `get_prefix_or_default` after
	/// the next call to `push`.
	///
	/// Returns whether the prefix is freshly declared and the resulting
	/// prefix.
	///
	/// This may return a non-auto-generated prefix if the namespace URI is
	/// already declared on this or a parent element. If the URI is already
	/// used for the default namespace, this function will nontheless return
	/// a prefix.
	fn declare_with_auto_prefix(&mut self, name: Option<NamespaceName>) -> (bool, &NcNameStr);

	/// Get the prefix for a given URI, which may be empty if the namespace
	/// with that URI is defined as the default namespace.
	fn get_prefix_or_default(
		&self,
		name: Option<NamespaceName>,
	) -> Result<Option<&NcNameStr>, PrefixError>;

	/// Get the prefix for a given URI.
	///
	/// This returns an error if the given URI is declared as default
	/// namespace and there is no matching prefix.
	fn get_prefix(&self, name: Option<NamespaceName>) -> Result<&NcNameStr, PrefixError>;

	/// Complete an element declaration.
	fn push(&mut self);

	/// Signal end of element to undeclare nested namespace declarations.
	fn pop(&mut self);

	/// Return the newly declared default namespace, if any.
	///
	/// This returns `None` if and only if no default namespace has been
	/// declared yet for the upcoming element.
	///
	/// `Some(None)` is returned if the empty default namespace has been
	/// declared, effectively resetting the default namespace to the initial
	/// state in an XML document.
	///
	/// *Note:* In the root element, `None` will be returned even if the
	/// default namespace has been explicitly set to `None`, as that is the
	/// default.
	fn new_default_declaration(&self) -> Option<Option<&NamespaceName>>;

	/// Return an iterator over the the newly declared prefixes.
	fn new_prefix_declarations<'x>(
		&'x self,
	) -> Box<dyn Iterator<Item = (&Option<NamespaceName>, &NcNameStr)> + 'x>;
}

/// Simple namespace tracker.
///
/// This is the default namespace tracker used by [`Encoder::new`]. At the
/// cost of increased output size, it reduces memory footprint by only
/// tracking unprefixed namespaces. Prefixed namespaces may be declared, but
/// are forgotten about once the element start is over.
///
/// Effectively, when used with an [`Encoder`], this means that prefixed
/// namespaces will only ever be used for attributes, and may be re-declared
/// a lot.
///
/// One exception is that prefixed namespaces declared on the root element
/// will actually be made available on all child elements.
#[derive(Debug)]
pub struct SimpleNamespaces {
	// persistent state
	global_ns: HashMap<Option<NamespaceName>, NcName>,
	global_ns_rev: HashSet<NcName>,
	global_ns_ctr: usize,
	default_ns_stack: Vec<Option<NamespaceName>>,

	// temporary per-element state
	next_default_ns: Option<Option<NamespaceName>>,
	temp_ns_ctr: usize,
	temp_ns: HashMap<Option<NamespaceName>, NcName>,
	temp_ns_rev: HashSet<NcName>,
}

impl SimpleNamespaces {
	pub fn new() -> Self {
		Self {
			global_ns: HashMap::new(),
			global_ns_rev: HashSet::new(),
			global_ns_ctr: 0,
			default_ns_stack: Vec::new(),
			// default default ns name is empty str
			next_default_ns: None,
			temp_ns_ctr: 0,
			temp_ns: HashMap::new(),
			temp_ns_rev: HashSet::new(),
		}
	}

	/// Look up the namespace URI for a given prefix
	///
	/// *Note:* This function is implemented as O(n) function because it
	/// should rarely, if ever, be necessary to use. Speeding up runtime of
	/// this function would increase memory cost at neglegible gain.
	///
	/// The namespace URIs for the `xml` and `xmlns` prefixes are always
	/// returned, even if not explicitly declared.
	pub fn lookup_prefix(&self, prefix: Option<&NcNameStr>) -> Result<NamespaceName, PrefixError> {
		match prefix {
			Some(prefix) if prefix == PREFIX_XML => Ok(RcPtr::new(XMLNS_XML.into())),
			Some(prefix) if prefix == PREFIX_XMLNS => Ok(RcPtr::new(XMLNS_XMLNS.into())),
			Some(prefix) => {
				for (decl_uri, decl_prefix) in self.temp_ns.iter().chain(self.global_ns.iter()) {
					if let Some(decl_uri) = decl_uri {
						if decl_prefix == prefix {
							return Ok(RcPtr::clone(decl_uri));
						}
					}
				}
				Err(PrefixError::Undeclared)
			}
			None => {
				match self.next_default_ns.as_ref() {
					Some(Some(uri)) => return Ok(RcPtr::clone(uri)),
					_ => (),
				};
				match self.default_ns_stack.last() {
					Some(Some(uri)) => Ok(RcPtr::clone(uri)),
					_ => Err(PrefixError::Undeclared),
				}
			}
		}
	}
}

impl TrackNamespace for SimpleNamespaces {
	fn declare_fixed(&mut self, prefix: Option<&NcNameStr>, name: Option<NamespaceName>) -> bool {
		match prefix.as_ref() {
			Some(v) if *v == PREFIX_XML => {
				if name.as_ref().map(|x| &***x) == Some(XMLNS_XML) {
					return false;
				}
				panic!("xml is a reserved prefix")
			}
			Some(v) if *v == PREFIX_XMLNS => {
				if name.as_ref().map(|x| &***x) == Some(XMLNS_XMLNS) {
					return false;
				}
				panic!("xmlns is a reserved prefix")
			}
			_ => {}
		}

		match name {
			Some(v) if *v == XMLNS_XML => {
				panic!("{} must be bound to xml prefix", *v)
			}
			Some(v) if *v == XMLNS_XMLNS => {
				panic!("{} must be bound to xmlns prefix", *v)
			}
			_ => {}
		}

		match prefix {
			Some(prefix) => {
				if self.global_ns_rev.contains(prefix) {
					panic!(
						"prefix declaration conflicts with global prefix: {:?}",
						prefix
					)
				}
				if self.temp_ns_rev.contains(prefix) {
					panic!("duplicate prefix: {:?}", prefix);
				}
				self.temp_ns.insert(name, prefix.to_ncname());
				self.temp_ns_rev.insert(prefix.to_ncname());
				true
			}
			None => {
				if self.next_default_ns.is_some() {
					panic!("duplicate default namespace")
				}
				self.next_default_ns = Some(name);
				true
			}
		}
	}

	fn declare_auto(&mut self, name: Option<NamespaceName>) -> (bool, Option<&NcNameStr>) {
		match name {
			Some(v) if *v == XMLNS_XML => return (false, Some(PREFIX_XML)),
			Some(v) if *v == XMLNS_XMLNS => return (false, Some(PREFIX_XMLNS)),
			_ => (),
		};

		// XXX: due to a limitation in the borrowchecker <https://github.com/rust-lang/rfcs/blob/master/text/2094-nll.md#problem-case-3-conditional-control-flow-across-functions>, we have to re-implement the entirety of get_prefix_or_default here.

		match self
			.next_default_ns
			.as_ref()
			.or(self.default_ns_stack.last())
		{
			Some(default_name) if *default_name == name => return (false, None),
			_ => (),
		};

		match self.temp_ns.entry(name.clone()) {
			Entry::Occupied(o) => (false, Some(o.into_mut())),
			Entry::Vacant(v_temp) => match self.global_ns.entry(name.clone()) {
				Entry::Occupied(o) => (false, Some(o.into_mut())),
				Entry::Vacant(_) => {
					match self.next_default_ns.as_ref() {
						// checked above already that it does not match
						Some(_) => {
							let ctr = self.temp_ns_ctr;
							let temp_ns_prefix: NcName = format!("tns{}", ctr)
								.try_into()
								.expect("auto-generated prefix must always be valid");
							if self.global_ns_rev.contains(&temp_ns_prefix) {
								panic!(
									"automatic prefix declaration conflicts with global prefix: {:?}",
									temp_ns_prefix
								)
							}
							if self.temp_ns_rev.contains(&temp_ns_prefix) {
								panic!(
									"automatic prefix declaration conflicts with local prefix: {:?}",
									temp_ns_prefix
								)
							}
							self.temp_ns_ctr += 1;
							self.temp_ns_rev.insert(temp_ns_prefix.clone());
							(true, Some(v_temp.insert(temp_ns_prefix)))
						}
						None => {
							self.next_default_ns = Some(name);
							let new = match self.default_ns_stack.last() {
								Some(v) => v != self.next_default_ns.as_ref().unwrap(),
								None => self.next_default_ns.as_ref().unwrap().is_some(),
							};
							(new, None)
						}
					}
				}
			},
		}
	}

	fn declare_with_auto_prefix(&mut self, name: Option<NamespaceName>) -> (bool, &NcNameStr) {
		match name {
			Some(v) if *v == XMLNS_XML => return (false, PREFIX_XML),
			Some(v) if *v == XMLNS_XMLNS => return (false, PREFIX_XMLNS),
			_ => (),
		}

		match self.temp_ns.entry(name) {
			Entry::Occupied(o) => (false, o.into_mut()),
			Entry::Vacant(v) => {
				let ctr = self.temp_ns_ctr;
				let temp_ns_prefix: NcName = format!("tns{}", ctr)
					.try_into()
					.expect("auto-generated prefix must always be valid");
				if self.global_ns_rev.contains(&temp_ns_prefix) {
					panic!(
						"automatic prefix declaration conflicts with global prefix: {:?}",
						temp_ns_prefix
					)
				}
				if self.temp_ns_rev.contains(&temp_ns_prefix) {
					panic!(
						"automatic prefix declaration conflicts with local prefix: {:?}",
						temp_ns_prefix
					)
				}
				self.temp_ns_ctr += 1;
				self.temp_ns_rev.insert(temp_ns_prefix.clone());
				(true, v.insert(temp_ns_prefix))
			}
		}
	}

	fn get_prefix_or_default(
		&self,
		name: Option<NamespaceName>,
	) -> Result<Option<&NcNameStr>, PrefixError> {
		match self
			.next_default_ns
			.as_ref()
			.or(self.default_ns_stack.last())
		{
			Some(default_name) if *default_name == name => Ok(None),
			_ => Ok(Some(self.get_prefix(name)?)),
		}
	}

	fn get_prefix(&self, name: Option<NamespaceName>) -> Result<&NcNameStr, PrefixError> {
		self.temp_ns
			.get(&name)
			.or(self.global_ns.get(&name))
			.map(|x| &**x)
			.ok_or(PrefixError::Undeclared)
	}

	fn push(&mut self) {
		match self.next_default_ns.take() {
			None => {
				// duplicate the most recent one
				// if there is no previous one, we go with the default namespace.
				let old = self.default_ns_stack.last().unwrap_or(&None).clone();
				self.default_ns_stack.push(old);
			}
			Some(v) => self.default_ns_stack.push(v),
		}
		if self.default_ns_stack.len() == 1 {
			// the first element! globalize the declarations
			std::mem::swap(&mut self.global_ns, &mut self.temp_ns);
			std::mem::swap(&mut self.global_ns_rev, &mut self.temp_ns_rev);
			self.global_ns_ctr = self.temp_ns_ctr;
		}

		self.temp_ns_ctr = self.global_ns_ctr;
		self.temp_ns.clear();
		self.temp_ns_rev.clear();
	}

	fn pop(&mut self) {
		self.default_ns_stack.pop();
	}

	fn new_default_declaration(&self) -> Option<Option<&NamespaceName>> {
		match self.next_default_ns.as_ref().map(|x| x.as_ref()) {
			// if this is the root element, we do not expose the empty default namespace as declaration
			Some(None) if self.default_ns_stack.len() == 0 => None,
			other => other,
		}
	}

	fn new_prefix_declarations<'x>(
		&'x self,
	) -> Box<dyn Iterator<Item = (&Option<NamespaceName>, &NcNameStr)> + 'x> {
		Box::new(self.temp_ns.iter().map(|(k, v)| (k, &**v)))
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
	/// Emitted if an XML declaration is placed after the first element
	/// started or if multiple XML declarations are placed.
	MisplacedXmlDeclaration,

	/// Emitted if any content is placed after the end of the last element.
	EndOfDocument,

	/// Emitted if text is placed inside an element heading
	TextNotAllowed,

	/// Emitted if attribute is placed outside of element heading
	AttributeNotAllowed,

	/// Emitted if element start is placed within element heading
	ElementStartNotAllowed,

	/// Emitted if element foot is placed before the first element
	ElementFootNotAllowed,

	/// Emitted on unbalanced element head start/end
	NoOpenElement,
}

impl fmt::Display for EncodeError {
	fn fmt<'f>(&self, f: &'f mut fmt::Formatter) -> fmt::Result {
		match self {
			Self::MisplacedXmlDeclaration => f.write_str("misplaced XML declaration"),
			Self::ElementStartNotAllowed => {
				f.write_str("element start not allowed inside element headers")
			}
			Self::NoOpenElement => f.write_str("no open element"),
			Self::EndOfDocument => f.write_str("no content allowed after end of root element"),
			Self::TextNotAllowed => f.write_str("text not allowed inside element headers"),
			Self::AttributeNotAllowed => {
				f.write_str("attributes not allowed outside element headers")
			}
			Self::ElementFootNotAllowed => f.write_str(
				"cannot close element while writing the header or before the root element",
			),
		}
	}
}

impl std::error::Error for EncodeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncoderState {
	Start,
	Declared,
	ElementHead,
	Content,
	EndOfDocument,
	CDataSection,
}

/**
Encodes XML into buffers.

Encoders are stateful. They can only be used to encode a single XML document and have then to be disposed.

```rust
use rxml::{Encoder, Item, XmlVersion};
use bytes::BytesMut;

let mut enc = Encoder::new();
let mut buf = BytesMut::new();
enc.encode(Item::XmlDeclaration(XmlVersion::V1_0), &mut buf);
assert_eq!(&buf[..], b"<?xml version='1.0' encoding='utf-8'?>\n");
```
*/
pub struct Encoder<T> {
	state: EncoderState,
	qname_stack: Vec<Name>,
	ns: T,
}

impl Encoder<SimpleNamespaces> {
	/// Create a new default encoder.
	///
	/// This encoder uses the [`SimpleNamespaces`] strategy, which is not
	/// optimal with respect to the number of bytes written, but has reduced
	/// memory cost.
	pub fn new() -> Self {
		Self {
			state: EncoderState::Start,
			qname_stack: Vec::new(),
			ns: SimpleNamespaces::new(),
		}
	}
}

impl<T: TrackNamespace> From<T> for Encoder<T> {
	/// Create an encoder from a [`TrackNamespace`] instance.
	///
	/// Use your own custom namespace strategy.
	fn from(ns: T) -> Self {
		Self {
			state: EncoderState::Start,
			qname_stack: Vec::new(),
			ns,
		}
	}
}

impl<T: TrackNamespace> Encoder<T> {
	fn encode_nsdecl<O: BufMut>(
		prefix: Option<&NcNameStr>,
		nsuri: Option<&CDataStr>,
		output: &mut O,
	) {
		match prefix {
			Some(prefix) => {
				output.put_slice(b" xmlns:");
				output.put_slice(prefix.as_bytes());
				output.put_slice(b"='");
			}
			None => {
				output.put_slice(b" xmlns='");
			}
		}
		if let Some(nsuri) = nsuri {
			escape(output, nsuri.as_bytes(), ATTR_SPECIALS);
		}
		output.put_u8(b'\'');
	}

	pub fn inner(&self) -> &T {
		&self.ns
	}

	pub fn inner_mut(&mut self) -> &mut T {
		&mut self.ns
	}

	/// Encode a single item into a buffer.
	///
	/// There is no requirement for the buffer to be the same for subsequent
	/// calls to this function. This allows users to use small, but
	/// long-lived, buffers for serialization before sending data over the
	/// network, for instance.
	pub fn encode<O: BufMut>(&mut self, item: Item<'_>, output: &mut O) -> Result<(), EncodeError> {
		if self.state == EncoderState::EndOfDocument {
			return Err(EncodeError::EndOfDocument);
		}

		match item {
			Item::XmlDeclaration(XmlVersion::V1_0) => match self.state {
				EncoderState::Start => {
					output.put_slice(XML_DECL);
					self.state = EncoderState::Declared;
					Ok(())
				}
				_ => Err(EncodeError::MisplacedXmlDeclaration),
			},
			Item::ElementHeadStart(nsuri, local_name) => match self.state {
				EncoderState::Start | EncoderState::Declared | EncoderState::Content => {
					output.put_u8(b'<');
					let (_, prefix) = self.ns.declare_auto(nsuri.clone());
					let qname = match prefix {
						Some(prefix) => {
							output.put_slice(prefix.as_bytes());
							output.put_u8(b':');
							output.put_slice(local_name.as_bytes());
							prefix.with_suffix(local_name)
						}
						None => {
							output.put_slice(local_name.as_bytes());
							local_name.to_name()
						}
					};
					self.qname_stack.push(qname);
					match self.ns.new_default_declaration() {
						Some(name) => {
							Self::encode_nsdecl(None, name.as_ref().map(|x| &****x), output)
						}
						None => (),
					};
					for (name, prefix) in self.ns.new_prefix_declarations() {
						// if new, we have to declare it
						Self::encode_nsdecl(Some(prefix), name.as_ref().map(|x| &***x), output);
					}
					self.state = EncoderState::ElementHead;
					Ok(())
				}
				_ => Err(EncodeError::ElementStartNotAllowed),
			},
			Item::Attribute(nsuri, local_name, value) => match self.state {
				EncoderState::ElementHead => {
					match nsuri {
						Some(v) => {
							let (new, prefix) = self.ns.declare_with_auto_prefix(Some(v.clone()));
							if new {
								Self::encode_nsdecl(Some(prefix), Some(&**v), output)
							}
							output.put_u8(b' ');
							output.put_slice(prefix.as_bytes());
							output.put_u8(b':');
							output.put_slice(local_name.as_bytes());
						}
						None => {
							output.put_u8(b' ');
							output.put_slice(local_name.as_bytes());
						}
					}
					output.put_u8(b'=');
					output.put_u8(b'"');
					escape(output, value.as_bytes(), &ATTR_SPECIALS);
					output.put_u8(b'"');
					Ok(())
				}
				_ => Err(EncodeError::AttributeNotAllowed),
			},
			Item::ElementHeadEnd => match self.state {
				EncoderState::ElementHead => {
					output.put_u8(b'>');
					self.ns.push();
					self.state = EncoderState::Content;
					Ok(())
				}
				_ => Err(EncodeError::NoOpenElement),
			},
			Item::CDataOpen => match self.state {
				EncoderState::Content => {
					self.state = EncoderState::CDataSection;
					output.put_slice(CDATAOPEN.as_bytes());
					Ok(())
				}
				_ => Err(EncodeError::TextNotAllowed),
			},
			Item::CDataClose => match self.state {
				EncoderState::CDataSection => {
					self.state = EncoderState::Content;
					output.put_slice(CDATACLOSE.as_bytes());
					Ok(())
				}
				_ => Err(EncodeError::TextNotAllowed),
			},
			Item::RawText(s) => match self.state {
				EncoderState::CDataSection => {
					output.put_slice(s.as_bytes());
					Ok(())
				}
				_ => Err(EncodeError::TextNotAllowed),
			},
			Item::Text(cdata) => match self.state {
				EncoderState::Content => {
					escape(output, cdata.as_bytes(), &CDATA_SPECIALS);
					Ok(())
				}
				_ => Err(EncodeError::TextNotAllowed),
			},
			Item::ElementFoot => match self.state {
				EncoderState::Content => {
					self.ns.pop();
					output.put_slice(b"</");
					output.put_slice(self.qname_stack.pop().unwrap().as_bytes());
					output.put_u8(b'>');
					if self.qname_stack.len() == 0 {
						self.state = EncoderState::EndOfDocument
					}
					Ok(())
				}
				EncoderState::ElementHead => {
					output.put_slice(b"/>");
					self.ns.push();
					self.ns.pop();
					self.qname_stack.pop();
					if self.qname_stack.len() == 0 {
						self.state = EncoderState::EndOfDocument
					} else {
						self.state = EncoderState::Content;
					}
					Ok(())
				}
				_ => Err(EncodeError::ElementFootNotAllowed),
			},
		}
	}

	/// Encode a single item into a BytesMut.
	///
	/// This might have a slight performance advantage over
	/// [`encode`] in some scenarios, as it might be able to give
	/// the BytesMut a heads up about required space, thus avoiding frequent
	/// reallocations.
	///
	/// There is no requirement for the buffer to be the same for subsequent
	/// calls to this function. This allows users to use small, but
	/// long-lived, buffers for serialization before sending data over the
	/// network, for instance.
	///
	///   [`encode`]: Self::encode
	pub fn encode_into_bytes(
		&mut self,
		item: Item<'_>,
		output: &mut BytesMut,
	) -> Result<(), EncodeError> {
		self.encode(item, output)
	}

	/// Encode a single event into a buffer.
	///
	/// This internally decomposes the event into multiple items and then
	/// encodes these into the given buffer using [`encode`].
	///
	///    [`encode`]: Self::encode.
	pub fn encode_event<O: BufMut>(
		&mut self,
		ev: &ResolvedEvent,
		output: &mut O,
	) -> Result<(), EncodeError> {
		match ev {
			ResolvedEvent::XmlDeclaration(_, version) => {
				self.encode(Item::XmlDeclaration(*version), output)?;
			}
			ResolvedEvent::StartElement(_, (ns, name), attrs) => {
				self.encode(Item::ElementHeadStart(ns.clone(), name.as_ref()), output)?;
				for ((ns, name), v) in attrs.iter() {
					self.encode(
						Item::Attribute(ns.clone(), name.as_ref(), v.as_ref()),
						output,
					)?
				}
				self.encode(Item::ElementHeadEnd, output)?;
			}
			ResolvedEvent::EndElement(_) => self.encode(Item::ElementFoot, output)?,
			ResolvedEvent::Text(_, text) => self.encode(Item::Text(text.as_ref()), output)?,
		}
		Ok(())
	}

	/// Encode a single event into a BytesMut.
	///
	/// This internally decomposes the event into multiple items and then
	/// encodes these into the given buffer using [`encode_into_bytes`].
	///
	///    [`encode_into_bytes`]: Self::encode_into_bytes.
	pub fn encode_event_into_bytes(
		&mut self,
		ev: &ResolvedEvent,
		output: &mut BytesMut,
	) -> Result<(), EncodeError> {
		self.encode_event(ev, output)
	}
}

#[cfg(test)]
mod tests_simple_namespaces {
	use super::*;

	use std::convert::TryFrom;

	fn ns1() -> NamespaceName {
		RcPtr::new(CData::try_from("uri:foo").unwrap())
	}

	fn ns2() -> NamespaceName {
		RcPtr::new(CData::try_from("uri:bar").unwrap())
	}

	fn ns3() -> NamespaceName {
		RcPtr::new(CData::try_from("uri:baz").unwrap())
	}

	fn mk() -> SimpleNamespaces {
		SimpleNamespaces::new()
	}

	#[test]
	fn prefers_setting_default_namespace() {
		let mut ns = mk();
		let (new, prefix) = ns.declare_auto(Some(ns1()));
		assert!(new);
		assert!(prefix.is_none());
	}

	#[test]
	fn unset_namespace_is_not_new_initially() {
		let mut ns = mk();
		let (new, prefix) = ns.declare_auto(None);
		assert!(!new);
		assert!(prefix.is_none());
	}

	#[test]
	fn predefined_prefixes_can_be_looked_up() {
		let ns = mk();
		assert_eq!(*ns.lookup_prefix(Some(PREFIX_XML)).unwrap(), XMLNS_XML);
		assert_eq!(*ns.lookup_prefix(Some(PREFIX_XMLNS)).unwrap(), XMLNS_XMLNS);
	}

	#[test]
	fn hardcoded_xmlns_namespace_for_declare_auto() {
		let mut ns = mk();
		let (new, prefix) = ns.declare_auto(Some(RcPtr::new(XMLNS_XMLNS.to_cdata())));
		assert!(!new);
		assert!(prefix.unwrap() == PREFIX_XMLNS);
	}

	#[test]
	fn hardcoded_xml_namespace_for_declare_auto() {
		let mut ns = mk();
		let (new, prefix) = ns.declare_auto(Some(RcPtr::new(XMLNS_XML.to_cdata())));
		assert!(!new);
		assert!(prefix.unwrap() == PREFIX_XML);
	}

	#[test]
	fn hardcoded_xmlns_namespace_for_declare_with_auto_prefix() {
		let mut ns = mk();
		let (new, prefix) = ns.declare_with_auto_prefix(Some(RcPtr::new(XMLNS_XMLNS.to_cdata())));
		assert!(!new);
		assert!(prefix == "xmlns");
	}

	#[test]
	fn hardcoded_xml_namespace_for_declare_with_auto_prefix() {
		let mut ns = mk();
		let (new, prefix) = ns.declare_with_auto_prefix(Some(RcPtr::new(XMLNS_XML.to_cdata())));
		assert!(!new);
		assert!(prefix == "xml");
	}

	#[test]
	fn hardcoded_xmlns_namespace_for_declare_fixed() {
		let mut ns = mk();
		let new = ns.declare_fixed(Some(PREFIX_XMLNS), Some(RcPtr::new(XMLNS_XMLNS.to_cdata())));
		assert!(!new);
	}

	#[test]
	fn hardcoded_xml_namespace_for_declare_fixed() {
		let mut ns = mk();
		let new = ns.declare_fixed(Some(PREFIX_XML), Some(RcPtr::new(XMLNS_XML.to_cdata())));
		assert!(!new);
	}

	#[test]
	#[should_panic(expected = "xml is a reserved prefix")]
	fn reject_xml_as_fixed_prefix_for_random_namespace() {
		let mut ns = mk();
		ns.declare_fixed(Some(PREFIX_XML), Some(ns1()));
	}

	#[test]
	#[should_panic(expected = "xmlns is a reserved prefix")]
	fn reject_xmlns_fixed_prefix_for_random_namespace() {
		let mut ns = mk();
		ns.declare_fixed(Some(PREFIX_XMLNS), Some(ns1()));
	}

	#[test]
	#[should_panic(expected = "must be bound to xml prefix")]
	fn reject_xml_namespace_with_other_prefix() {
		let mut ns = mk();
		ns.declare_fixed(
			Some("foo".try_into().unwrap()),
			Some(RcPtr::new(XMLNS_XML.to_cdata())),
		);
	}

	#[test]
	#[should_panic(expected = "must be bound to xmlns prefix")]
	fn reject_xmlns_namespace_with_other_prefix() {
		let mut ns = mk();
		ns.declare_fixed(
			Some("foo".try_into().unwrap()),
			Some(RcPtr::new(XMLNS_XMLNS.to_cdata())),
		);
	}

	#[test]
	fn retrieve_default_namespace() {
		let mut ns = mk();
		ns.declare_auto(Some(ns1()));
		match ns.get_prefix_or_default(Some(ns1())) {
			Ok(None) => {}
			other => panic!("unexpected get_prefix_or_default result: {:?}", other),
		};
		match ns.lookup_prefix(None) {
			Ok(name) => {
				assert_eq!(name, ns1());
			}
			other => panic!("unexpected lookup_prefix result: {:?}", other),
		}
	}

	#[test]
	fn declaration_causes_lookup_to_return_a_thing() {
		let mut ns = mk();
		match ns.lookup_prefix(None) {
			Err(PrefixError::Undeclared) => (),
			other => panic!("unexpected lookup_prefix result: {:?}", other),
		}
		ns.declare_auto(Some(ns1()));
		match ns.lookup_prefix(None) {
			Ok(name) => {
				assert_eq!(name, ns1());
			}
			other => panic!("unexpected lookup_prefix result: {:?}", other),
		}
	}

	#[test]
	fn reuses_existing_declaration_if_available() {
		let mut ns = mk();
		ns.declare_auto(Some(ns1()));
		let (new, prefix) = ns.declare_auto(Some(ns1()));
		assert!(!new);
		assert!(prefix.is_none());
	}

	#[test]
	fn allocates_prefix_if_default_is_already_set() {
		let mut ns = mk();
		ns.declare_auto(Some(ns1()));
		let (new, prefix) = ns.declare_auto(Some(ns2()));
		assert!(new);
		assert!(prefix.is_some());
	}

	#[test]
	fn retrieve_prefixed_namespace() {
		let mut ns = mk();
		let prefix = ns.declare_with_auto_prefix(Some(ns1())).1.to_ncname();
		match ns.get_prefix_or_default(Some(ns1())) {
			Ok(Some(v)) => {
				assert_eq!(v, prefix);
			}
			other => panic!("unexpected get_prefix_or_default result: {:?}", other),
		};
	}

	#[test]
	fn declaration_causes_lookup_to_return_a_thing_for_a_prefix() {
		let mut ns = mk();
		match ns.lookup_prefix(None) {
			Err(PrefixError::Undeclared) => (),
			other => panic!("unexpected lookup_prefix result: {:?}", other),
		}
		let prefix = ns.declare_with_auto_prefix(Some(ns1())).1.to_ncname();
		match ns.lookup_prefix(Some(&prefix)) {
			Ok(name) => {
				assert_eq!(name, ns1());
			}
			other => panic!("unexpected lookup_prefix result: {:?}", other),
		}
		match ns.lookup_prefix(None) {
			Err(PrefixError::Undeclared) => (),
			other => panic!("unexpected lookup_prefix result: {:?}", other),
		}
	}

	#[test]
	fn reuses_auto_allocated_prefix_for_same_ns() {
		let mut ns = mk();
		let prefix1 = ns.declare_with_auto_prefix(Some(ns1())).1.to_ncname();
		let prefix2 = ns.declare_with_auto_prefix(Some(ns1())).1.to_ncname();
		assert_eq!(prefix1, prefix2);
	}

	#[test]
	fn auto_allocates_different_prefixes_for_different_ns() {
		let mut ns = mk();
		let prefix1 = ns.declare_with_auto_prefix(Some(ns1())).1.to_ncname();
		let prefix2 = ns.declare_with_auto_prefix(Some(ns2())).1.to_ncname();
		assert_ne!(prefix1, prefix2);
	}

	#[test]
	fn preserves_default_ns_across_elements() {
		let mut ns = mk();
		ns.declare_auto(Some(ns1()));
		ns.get_prefix_or_default(Some(ns1())).unwrap();
		ns.push();
		let (new, prefix) = ns.declare_auto(Some(ns1()));
		assert!(!new);
		assert!(prefix.is_none());
		assert!(ns.get_prefix_or_default(Some(ns1())).unwrap().is_none());
		match ns.lookup_prefix(None) {
			Ok(name) => {
				assert_eq!(name, ns1());
			}
			other => panic!("unexpected lookup_prefix result: {:?}", other),
		}
	}

	#[test]
	fn declare_with_fixed_prefix() {
		let mut ns = mk();
		ns.declare_fixed(Some(NcNameStr::from_str("stream").unwrap()), Some(ns1()));
		match ns.get_prefix(Some(ns1())) {
			Ok(v) => {
				assert_eq!(v, "stream");
			}
			other => panic!("unexpected get_prefix result: {:?}", other),
		}
		match ns.get_prefix_or_default(Some(ns1())) {
			Ok(Some(v)) => {
				assert_eq!(v, "stream");
			}
			other => panic!("unexpected get_prefix result: {:?}", other),
		}
	}

	#[test]
	fn declare_fixed_prefixless() {
		let mut ns = mk();
		ns.declare_fixed(None, Some(ns1()));
		match ns.get_prefix_or_default(Some(ns1())) {
			Ok(None) => (),
			other => panic!("unexpected get_prefix result: {:?}", other),
		}
		match ns.get_prefix(Some(ns1())) {
			Err(PrefixError::Undeclared) => (),
			other => panic!("unexpected get_prefix result: {:?}", other),
		}
	}

	#[test]
	fn preserves_global_prefixed_across_elements() {
		let mut ns = mk();
		ns.declare_fixed(Some(NcNameStr::from_str("stream").unwrap()), Some(ns1()));
		ns.declare_fixed(None, Some(ns2()));
		ns.push();
		match ns.get_prefix(Some(ns1())) {
			Ok(v) => {
				assert_eq!(v, "stream");
			}
			other => panic!("unexpected get_prefix result: {:?}", other),
		}
		match ns.get_prefix_or_default(Some(ns1())) {
			Ok(Some(v)) => {
				assert_eq!(v, "stream");
			}
			other => panic!("unexpected get_prefix result: {:?}", other),
		}
		match ns.get_prefix_or_default(Some(ns2())) {
			Ok(None) => (),
			other => panic!("unexpected get_prefix result: {:?}", other),
		}
		match ns.get_prefix(Some(ns2())) {
			Err(PrefixError::Undeclared) => (),
			other => panic!("unexpected get_prefix result: {:?}", other),
		}
	}

	#[test]
	#[should_panic(expected = "conflict")]
	fn prohibits_overriding_global_prefix() {
		let mut ns = mk();
		ns.declare_fixed(Some(NcNameStr::from_str("stream").unwrap()), Some(ns1()));
		ns.declare_fixed(None, Some(ns2()));
		ns.push();
		ns.declare_fixed(Some(NcNameStr::from_str("stream").unwrap()), Some(ns2()));
	}

	#[test]
	fn auto_allocations_stay_global_and_do_not_cause_conflicts() {
		let mut ns = mk();
		let prefix1 = ns.declare_with_auto_prefix(Some(ns1())).1.to_ncname();
		let prefix2 = ns.declare_with_auto_prefix(Some(ns2())).1.to_ncname();
		ns.push();
		let (new, prefix3) = ns.declare_with_auto_prefix(Some(ns3()));
		assert!(new);
		let prefix3 = prefix3.to_ncname();
		assert_ne!(prefix1, prefix3);
		assert_ne!(prefix2, prefix3);
	}

	#[test]
	#[should_panic(expected = "conflict")]
	fn fixed_global_decl_can_conflict_with_auto_decl() {
		let mut ns = mk();
		ns.declare_fixed(Some(NcNameStr::from_str("tns0").unwrap()), Some(ns1()));
		ns.push();
		ns.declare_with_auto_prefix(Some(ns3()));
	}

	#[test]
	#[should_panic(expected = "conflict")]
	fn fixed_local_decl_can_conflict_with_auto_decl() {
		let mut ns = mk();
		ns.declare_fixed(Some(NcNameStr::from_str("tns0").unwrap()), Some(ns1()));
		ns.declare_with_auto_prefix(Some(ns3()));
	}
}

#[cfg(test)]
mod tests_encoder {
	use super::*;

	use std::convert::TryFrom;

	use crate::parser::EventMetrics;

	use crate::EventRead;

	fn mkencoder() -> Encoder<SimpleNamespaces> {
		Encoder::new()
	}

	fn ns1() -> NamespaceName {
		RcPtr::new(CData::try_from("uri:foo").unwrap())
	}

	fn ns2() -> NamespaceName {
		RcPtr::new(CData::try_from("uri:bar").unwrap())
	}

	fn ns3() -> NamespaceName {
		RcPtr::new(CData::try_from("uri:baz").unwrap())
	}

	fn parse(mut input: &[u8]) -> (Vec<ResolvedEvent>, crate::Result<bool>) {
		let mut parser = crate::PullParser::new(&mut input);
		let mut events = Vec::new();
		let result = parser.read_all_eof(|ev| events.push(ev));
		(events, result)
	}

	fn encode_events(evs: &[ResolvedEvent]) -> Result<BytesMut, EncodeError> {
		let mut out = BytesMut::new();
		let mut encoder = mkencoder();
		for ev in evs {
			encoder.encode_event(&ev, &mut out)?;
		}
		Ok(out)
	}

	fn encode_events_via_into_bytes(evs: &[ResolvedEvent]) -> Result<BytesMut, EncodeError> {
		let mut out = BytesMut::new();
		let mut encoder = mkencoder();
		for ev in evs {
			encoder.encode_event_into_bytes(&ev, &mut out)?;
		}
		Ok(out)
	}

	fn collapse_cdata(evs: &mut Vec<ResolvedEvent>) {
		let mut buf = Vec::new();
		std::mem::swap(&mut buf, evs);
		let mut cdata_hold = None;
		for event in buf.drain(..) {
			match event {
				ResolvedEvent::Text(_, txt) => match cdata_hold.take() {
					None => cdata_hold = Some(txt),
					Some(existing) => cdata_hold = Some(existing + &*txt),
				},
				_ => {
					match cdata_hold.take() {
						Some(txt) => evs.push(ResolvedEvent::Text(EventMetrics::new(0), txt)),
						None => (),
					};
					evs.push(event);
				}
			}
		}
		match cdata_hold.take() {
			Some(txt) => evs.push(ResolvedEvent::Text(EventMetrics::new(0), txt)),
			None => (),
		};
	}

	fn assert_event_eq(a: &ResolvedEvent, b: &ResolvedEvent) {
		match (a, b) {
			(ResolvedEvent::XmlDeclaration(_, v1), ResolvedEvent::XmlDeclaration(_, v2)) => {
				assert_eq!(v1, v2);
			}
			(
				ResolvedEvent::StartElement(_, name1, attrs1),
				ResolvedEvent::StartElement(_, name2, attrs2),
			) => {
				assert_eq!(name1, name2);
				assert_eq!(attrs1, attrs2);
			}
			(ResolvedEvent::EndElement(_), ResolvedEvent::EndElement(_)) => {}
			(ResolvedEvent::Text(_, text1), ResolvedEvent::Text(_, text2)) => {
				assert_eq!(text1, text2);
			}
			// will always raise
			(a, b) => panic!("event types differ: {:?} != {:?}", a, b),
		}
	}

	fn assert_events_eq(initial: &[ResolvedEvent], reparsed: &[ResolvedEvent]) {
		for (a, b) in initial.iter().zip(reparsed.iter()) {
			assert_event_eq(a, b);
		}
		if initial.len() > reparsed.len() {
			panic!(
				"missing {} events in reparsed version",
				initial.len() - reparsed.len()
			)
		}
		if reparsed.len() > initial.len() {
			panic!(
				"{} additional events in reparsed version: {:?}",
				reparsed.len() - initial.len(),
				&reparsed[initial.len()..]
			)
		}
	}

	fn check_reserialized(
		input: &[u8],
		initial: &[ResolvedEvent],
		initial_eof: bool,
		reserialized: &[u8],
		via: &'static str,
	) {
		let (mut reparsed, reparsed_result) = parse(&reserialized[..]);
		collapse_cdata(&mut reparsed);
		let reparsed_eof = match reparsed_result {
			Ok(eof) => eof,
			Err(e) => {
				panic!(
					"reserialized (via {}) XML\n\n{:?}\n\n  of\n\n{:?}\n\nfails to parse: {}",
					via,
					String::from_utf8_lossy(&reserialized[..]),
					String::from_utf8_lossy(input),
					e
				)
			}
		};
		println!("checking (via {})", via);
		println!(
			"reserialized: {:?}",
			String::from_utf8_lossy(&reserialized[..])
		);
		assert_events_eq(initial, &reparsed);
		assert_eq!(initial_eof, reparsed_eof);
	}

	fn roundtrip_test(input: &[u8]) {
		// goal: test that a parsed thing can be serialized again and then parsed to the semantically equivalent series of events
		let (mut initial, initial_result) = parse(input);
		collapse_cdata(&mut initial);
		let initial_eof = initial_result.expect("test input must parse correctly");
		let reserialized_via_buf =
			encode_events(&initial[..]).expect("parsed input must be encodable");
		let reserialized_via_bytes =
			encode_events_via_into_bytes(&initial[..]).expect("parsed input must be encodable");
		check_reserialized(
			input,
			&initial,
			initial_eof,
			&reserialized_via_buf[..],
			"buf",
		);
		check_reserialized(
			input,
			&initial,
			initial_eof,
			&reserialized_via_bytes[..],
			"bytes",
		);
	}

	#[test]
	fn reject_duplicate_xml_declaration() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		match enc.encode(Item::XmlDeclaration(XmlVersion::V1_0), &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::XmlDeclaration(XmlVersion::V1_0), &mut buf) {
			Err(EncodeError::MisplacedXmlDeclaration) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
	}

	#[test]
	fn reject_text_at_global_level() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		match enc.encode(Item::Text("".try_into().unwrap()), &mut buf) {
			Err(EncodeError::TextNotAllowed) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
	}

	#[test]
	fn reject_attribute_at_global_level() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		match enc.encode(
			Item::Attribute(None, "x".try_into().unwrap(), "".try_into().unwrap()),
			&mut buf,
		) {
			Err(EncodeError::AttributeNotAllowed) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
	}

	#[test]
	fn allow_element_before_decl() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		match enc.encode(
			Item::ElementHeadStart(None, "x".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
	}

	#[test]
	fn reject_xml_decl_in_element() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		match enc.encode(
			Item::ElementHeadStart(None, "x".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::XmlDeclaration(XmlVersion::V1_0), &mut buf) {
			Err(EncodeError::MisplacedXmlDeclaration) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
	}

	#[test]
	fn encode_self_closed_tag() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		match enc.encode(
			Item::ElementHeadStart(None, "x".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementFoot, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		assert_eq!(&buf, &b"<x/>"[..]);
	}

	#[test]
	fn encode_cdata_section() {
		let mut enc = mkencoder();
		let mut buf = Vec::new();
		enc.encode(
			Item::ElementHeadStart(None, "x".try_into().unwrap()),
			&mut buf,
		)
		.unwrap();
		enc.encode(Item::ElementHeadEnd, &mut buf).unwrap();
		enc.encode(Item::CDataOpen, &mut buf).unwrap();
		enc.encode(Item::RawText("this has special chars like &"), &mut buf)
			.unwrap();
		enc.encode(Item::CDataClose, &mut buf).unwrap();
		enc.encode(Item::ElementFoot, &mut buf).unwrap();
		assert_eq!(
			String::from_utf8(buf).unwrap().as_str(),
			"<x><![CDATA[this has special chars like &]]></x>"
		);
	}

	#[test]
	fn encode_root_prefix() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		enc.ns
			.declare_fixed(Some("foo".try_into().unwrap()), Some(ns1()));
		match enc.encode(
			Item::ElementHeadStart(Some(ns1()), "x".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementFoot, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		assert_eq!(&buf, &b"<foo:x xmlns:foo='uri:foo'/>"[..]);
	}

	#[test]
	fn use_explicitly_set_at_root() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		enc.ns
			.declare_fixed(Some("foo".try_into().unwrap()), Some(ns1()));
		match enc.encode(
			Item::ElementHeadStart(Some(ns2()), "x".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementHeadEnd, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(
			Item::ElementHeadStart(Some(ns1()), "y".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementFoot, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementFoot, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		assert_eq!(
			&buf,
			&b"<x xmlns='uri:bar' xmlns:foo='uri:foo'><foo:y/></x>"[..]
		);
	}

	#[test]
	fn do_not_emit_duplicate_declarations() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		enc.ns
			.declare_fixed(Some("foo".try_into().unwrap()), Some(ns1()));
		match enc.encode(
			Item::ElementHeadStart(Some(ns2()), "x".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(
			Item::Attribute(
				Some(ns1()),
				"a1".try_into().unwrap(),
				"v1".try_into().unwrap(),
			),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		}
		match enc.encode(
			Item::Attribute(
				Some(ns1()),
				"a2".try_into().unwrap(),
				"v2".try_into().unwrap(),
			),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		}
		match enc.encode(
			Item::Attribute(
				Some(ns2()),
				"a3".try_into().unwrap(),
				"v3".try_into().unwrap(),
			),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		}
		match enc.encode(
			Item::Attribute(
				Some(ns2()),
				"a4".try_into().unwrap(),
				"v4".try_into().unwrap(),
			),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		}
		match enc.encode(
			Item::Attribute(
				Some(ns3()),
				"a5".try_into().unwrap(),
				"v5".try_into().unwrap(),
			),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		}
		match enc.encode(
			Item::Attribute(
				Some(ns3()),
				"a6".try_into().unwrap(),
				"v6".try_into().unwrap(),
			),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		}
		match enc.encode(Item::ElementHeadEnd, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(
			Item::ElementHeadStart(Some(ns1()), "y".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementFoot, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementFoot, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		assert_eq!(&buf, &b"<x xmlns='uri:bar' xmlns:foo='uri:foo' foo:a1=\"v1\" foo:a2=\"v2\" xmlns:tns0='uri:bar' tns0:a3=\"v3\" tns0:a4=\"v4\" xmlns:tns1='uri:baz' tns1:a5=\"v5\" tns1:a6=\"v6\"><foo:y/></x>"[..]);
	}

	#[test]
	fn self_closed_tag_encoding_ends_document_if_it_closes_the_root() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		match enc.encode(
			Item::ElementHeadStart(None, "x".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementFoot, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::Text("".try_into().unwrap()), &mut buf) {
			Err(EncodeError::EndOfDocument) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
	}

	#[test]
	fn self_closed_tag_encoding_allows_content_thereafter() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		match enc.encode(
			Item::ElementHeadStart(None, "x".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementHeadEnd, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(
			Item::ElementHeadStart(None, "y".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementFoot, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::Text("foo".try_into().unwrap()), &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		assert_eq!(&buf[..], b"<x><y/>foo");
	}

	#[test]
	fn reject_element_after_end_of_document() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		match enc.encode(
			Item::ElementHeadStart(None, "x".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementHeadEnd, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementFoot, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(
			Item::ElementHeadStart(None, "x".try_into().unwrap()),
			&mut buf,
		) {
			Err(EncodeError::EndOfDocument) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
	}

	#[test]
	fn reject_element_foot_before_start() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		match enc.encode(Item::ElementFoot, &mut buf) {
			Err(EncodeError::ElementFootNotAllowed) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
	}

	#[test]
	fn reject_element_head_end_outside_element_header() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		match enc.encode(Item::ElementHeadEnd, &mut buf) {
			Err(EncodeError::NoOpenElement) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(
			Item::ElementHeadStart(None, "x".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementHeadEnd, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementHeadEnd, &mut buf) {
			Err(EncodeError::NoOpenElement) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
	}

	#[test]
	fn reject_text_after_end_of_document() {
		let mut enc = mkencoder();
		let mut buf = BytesMut::new();
		match enc.encode(
			Item::ElementHeadStart(None, "x".try_into().unwrap()),
			&mut buf,
		) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementHeadEnd, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::ElementFoot, &mut buf) {
			Ok(()) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
		match enc.encode(Item::Text("".try_into().unwrap()), &mut buf) {
			Err(EncodeError::EndOfDocument) => (),
			other => panic!("unexpected encode result: {:?}", other),
		};
	}

	#[test]
	fn single_element_roundtrip() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a/>")
	}

	#[test]
	fn nested_element_roundtrip() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a><b/></a>")
	}

	#[test]
	fn mixed_content_roundtrip() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a>foo<b>bar</b>baz</a>")
	}

	#[test]
	fn text_with_lt_roundtrip() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a>&lt;</a>")
	}

	#[test]
	fn text_with_gt_roundtrip() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a>&gt;</a>")
	}

	#[test]
	fn text_with_escaped_entity_roundtrip() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a>&amp;amp;</a>")
	}

	#[test]
	fn text_cdata_sequence_roundtrip() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a>]]&gt;</a>")
	}

	#[test]
	fn attribute_roundtrip() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a a1='foo' a2=\"bar\"/>")
	}

	#[test]
	fn attribute_whitespace_roundtrip() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a a1='&#xd;&#xa;&#x9; '/>")
	}

	#[test]
	fn attribute_quotes_roundtrip() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a a1='&quot;&apos;'/>")
	}

	#[test]
	fn namespace_roundtrip() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a xmlns='uri:foo'/>")
	}

	#[test]
	fn namespace_roundtrip_builtins() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a xml:lang='de'/>")
	}

	#[test]
	fn namespace_roundtrip_with_prefixes() {
		roundtrip_test(
			b"<?xml version='1.0'?>\n<a xmlns='uri:foo' xmlns:b='uri:bar'><b:b><c/></b:b></a>",
		)
	}

	#[test]
	fn roundtrip_escaped_crlf() {
		roundtrip_test(b"<?xml version='1.0'?>\n<a>\r\n&#xd;&#xa;</a>")
	}

	#[test]
	fn prefixed_attribute_roundtrip() {
		roundtrip_test(
			b"<?xml version='1.0'?>\n<a xmlns:b='uri:foo' b:a1='baz' a2='fnord' b:a3='foobar'/>",
		)
	}
}

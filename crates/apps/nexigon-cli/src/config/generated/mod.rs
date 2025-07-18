/* GENERATED WITH SIDEX. DO NOT MODIFY! */

#![allow(warnings)]

pub mod config {
    #![doc = ""]
    #[allow(unused)]
    use :: serde as __serde;
    #[allow(unused)]
    use :: sidex_serde as __sidex_serde;
    #[doc = "User token.\n"]
    pub type UserToken = nexigon_ids::ids::UserToken;
    #[doc = "Filesystem path.\n"]
    pub type PathBuf = std::path::PathBuf;
    #[doc = "CLI configuration.\n"]
    #[derive(Clone, Debug)]
    pub struct Config {
        #[doc = "URL of the Nexigon Hub server.\n"]
        pub hub_url: ::std::string::String,
        #[doc = "User token.\n"]
        pub token: UserToken,
    }
    impl Config {
        #[doc = "Creates a new [`Config`]."]
        pub fn new(hub_url: ::std::string::String, token: UserToken) -> Self {
            Self { hub_url, token }
        }
        #[doc = "Sets the value of `hub_url`."]
        pub fn set_hub_url(&mut self, hub_url: ::std::string::String) -> &mut Self {
            self.hub_url = hub_url;
            self
        }
        #[doc = "Sets the value of `hub_url`."]
        pub fn with_hub_url(mut self, hub_url: ::std::string::String) -> Self {
            self.hub_url = hub_url;
            self
        }
        #[doc = "Sets the value of `token`."]
        pub fn set_token(&mut self, token: UserToken) -> &mut Self {
            self.token = token;
            self
        }
        #[doc = "Sets the value of `token`."]
        pub fn with_token(mut self, token: UserToken) -> Self {
            self.token = token;
            self
        }
    }
    #[automatically_derived]
    impl __serde::Serialize for Config {
        fn serialize<__S: __serde::Serializer>(
            &self,
            __serializer: __S,
        ) -> ::std::result::Result<__S::Ok, __S::Error> {
            let mut __record =
                __sidex_serde::ser::RecordSerializer::new(__serializer, "Config", 2usize)?;
            __record.serialize_field("hub-url", &self.hub_url)?;
            __record.serialize_field("token", &self.token)?;
            __record.end()
        }
    }
    #[automatically_derived]
    impl<'de> __serde::Deserialize<'de> for Config {
        fn deserialize<__D: __serde::Deserializer<'de>>(
            __deserializer: __D,
        ) -> ::std::result::Result<Self, __D::Error> {
            #[doc(hidden)]
            struct __Visitor {
                __phantom_vars: ::core::marker::PhantomData<fn(&())>,
            }
            impl<'de> __serde::de::Visitor<'de> for __Visitor {
                type Value = Config;
                fn expecting(
                    &self,
                    __formatter: &mut ::core::fmt::Formatter,
                ) -> ::core::fmt::Result {
                    ::core::fmt::Formatter::write_str(__formatter, "record Config")
                }
                #[inline]
                fn visit_seq<__A>(
                    self,
                    mut __seq: __A,
                ) -> ::core::result::Result<Self::Value, __A::Error>
                where
                    __A: __serde::de::SeqAccess<'de>,
                {
                    let __field0 = match __serde::de::SeqAccess::next_element::<
                        ::std::string::String,
                    >(&mut __seq)?
                    {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                __serde::de::Error::invalid_length(0usize, &"record with 2 fields"),
                            );
                        }
                    };
                    let __field1 =
                        match __serde::de::SeqAccess::next_element::<UserToken>(&mut __seq)? {
                            ::core::option::Option::Some(__value) => __value,
                            ::core::option::Option::None => {
                                return ::core::result::Result::Err(
                                    __serde::de::Error::invalid_length(
                                        1usize,
                                        &"record with 2 fields",
                                    ),
                                );
                            }
                        };
                    ::core::result::Result::Ok(Config {
                        hub_url: __field0,
                        token: __field1,
                    })
                }
                #[inline]
                fn visit_map<__A>(
                    self,
                    mut __map: __A,
                ) -> ::core::result::Result<Self::Value, __A::Error>
                where
                    __A: __serde::de::MapAccess<'de>,
                {
                    #[doc(hidden)]
                    const __IDENTIFIERS: &'static [&'static str] = &["hub-url", "token"];
                    #[doc(hidden)]
                    const __EXPECTING_IDENTIFIERS: &'static str =
                        "an identifier in [\"hub-url\", \"token\"]";
                    #[derive(:: core :: clone :: Clone, :: core :: marker :: Copy)]
                    #[doc(hidden)]
                    enum __Identifier {
                        __Identifier0,
                        __Identifier1,
                        __Unknown,
                    }
                    #[doc(hidden)]
                    struct __IdentifierVisitor;
                    impl<'de> __serde::de::Visitor<'de> for __IdentifierVisitor {
                        type Value = __Identifier;
                        fn expecting(
                            &self,
                            __formatter: &mut ::core::fmt::Formatter,
                        ) -> ::core::fmt::Result {
                            ::core::fmt::Formatter::write_str(__formatter, __EXPECTING_IDENTIFIERS)
                        }
                        fn visit_u64<__E>(
                            self,
                            __value: u64,
                        ) -> ::core::result::Result<Self::Value, __E>
                        where
                            __E: __serde::de::Error,
                        {
                            match __value {
                                0u64 => ::core::result::Result::Ok(__Identifier::__Identifier0),
                                1u64 => ::core::result::Result::Ok(__Identifier::__Identifier1),
                                _ => ::core::result::Result::Ok(__Identifier::__Unknown),
                            }
                        }
                        fn visit_str<__E>(
                            self,
                            __value: &str,
                        ) -> ::core::result::Result<Self::Value, __E>
                        where
                            __E: __serde::de::Error,
                        {
                            match __value {
                                "hub-url" => {
                                    ::core::result::Result::Ok(__Identifier::__Identifier0)
                                }
                                "token" => ::core::result::Result::Ok(__Identifier::__Identifier1),
                                _ => ::core::result::Result::Ok(__Identifier::__Unknown),
                            }
                        }
                        fn visit_bytes<__E>(
                            self,
                            __value: &[u8],
                        ) -> ::core::result::Result<Self::Value, __E>
                        where
                            __E: __serde::de::Error,
                        {
                            match __value {
                                b"hub-url" => {
                                    ::core::result::Result::Ok(__Identifier::__Identifier0)
                                }
                                b"token" => ::core::result::Result::Ok(__Identifier::__Identifier1),
                                _ => ::core::result::Result::Ok(__Identifier::__Unknown),
                            }
                        }
                    }
                    impl<'de> __serde::Deserialize<'de> for __Identifier {
                        #[inline]
                        fn deserialize<__D>(
                            __deserializer: __D,
                        ) -> ::core::result::Result<Self, __D::Error>
                        where
                            __D: __serde::Deserializer<'de>,
                        {
                            __serde::Deserializer::deserialize_identifier(
                                __deserializer,
                                __IdentifierVisitor,
                            )
                        }
                    }
                    let mut __field0: ::core::option::Option<::std::string::String> =
                        ::core::option::Option::None;
                    let mut __field1: ::core::option::Option<UserToken> =
                        ::core::option::Option::None;
                    while let ::core::option::Option::Some(__key) =
                        __serde::de::MapAccess::next_key::<__Identifier>(&mut __map)?
                    {
                        match __key {
                            __Identifier::__Identifier0 => {
                                if ::core::option::Option::is_some(&__field0) {
                                    return ::core::result::Result::Err(
                                        <__A::Error as __serde::de::Error>::duplicate_field(
                                            "hub-url",
                                        ),
                                    );
                                }
                                __field0 = ::core::option::Option::Some(
                                    __serde::de::MapAccess::next_value::<::std::string::String>(
                                        &mut __map,
                                    )?,
                                );
                            }
                            __Identifier::__Identifier1 => {
                                if ::core::option::Option::is_some(&__field1) {
                                    return ::core::result::Result::Err(
                                        <__A::Error as __serde::de::Error>::duplicate_field(
                                            "token",
                                        ),
                                    );
                                }
                                __field1 = ::core::option::Option::Some(
                                    __serde::de::MapAccess::next_value::<UserToken>(&mut __map)?,
                                );
                            }
                            _ => {
                                __serde::de::MapAccess::next_value::<__serde::de::IgnoredAny>(
                                    &mut __map,
                                )?;
                            }
                        }
                    }
                    let __field0 = match __field0 {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                <__A::Error as __serde::de::Error>::missing_field("hub-url"),
                            );
                        }
                    };
                    let __field1 = match __field1 {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                <__A::Error as __serde::de::Error>::missing_field("token"),
                            );
                        }
                    };
                    ::core::result::Result::Ok(Config {
                        hub_url: __field0,
                        token: __field1,
                    })
                }
            }
            #[doc(hidden)]
            const __FIELDS: &'static [&'static str] = &["hub-url", "token"];
            __serde::Deserializer::deserialize_struct(
                __deserializer,
                "Config",
                __FIELDS,
                __Visitor {
                    __phantom_vars: ::core::marker::PhantomData,
                },
            )
        }
    }
}

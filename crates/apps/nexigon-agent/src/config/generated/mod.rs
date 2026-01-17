/* GENERATED WITH SIDEX. DO NOT MODIFY! */

pub mod config {
    #![doc = ""]
    #[allow(unused)]
    use :: serde as __serde;
    #[allow(unused)]
    use :: sidex_serde as __sidex_serde;
    #[doc = "Deployment token.\n"]
    pub type DeploymentToken = nexigon_ids::ids::DeploymentToken;
    #[doc = "Filesystem path.\n"]
    pub type PathBuf = std::path::PathBuf;
    #[doc = "Agent configuration.\n"]
    #[derive(Clone, Debug)]
    pub struct Config {
        #[doc = "URL of the Nexigon Hub server.\n"]
        pub hub_url: ::std::string::String,
        #[doc = "Deployment token.\n"]
        pub token: DeploymentToken,
        #[doc = "Fingerprint script.\n"]
        pub fingerprint_script: PathBuf,
        #[doc = "Path to the device certificate.\n"]
        pub ssl_cert: ::std::option::Option<PathBuf>,
        #[doc = "Path to the device private key.\n"]
        pub ssl_key: ::std::option::Option<PathBuf>,
        #[doc = "Disable TLS.\n"]
        pub dangerous_disable_tls: ::std::option::Option<bool>,
        #[doc = "Exported services.\n"]
        pub exports: ::std::option::Option<::std::vec::Vec<ExportConfig>>,
    }
    impl Config {
        #[doc = "Creates a new [`Config`]."]
        pub fn new(
            hub_url: ::std::string::String,
            token: DeploymentToken,
            fingerprint_script: PathBuf,
        ) -> Self {
            Self {
                hub_url,
                token,
                fingerprint_script,
                ssl_cert: ::std::default::Default::default(),
                ssl_key: ::std::default::Default::default(),
                dangerous_disable_tls: ::std::default::Default::default(),
                exports: ::std::default::Default::default(),
            }
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
        pub fn set_token(&mut self, token: DeploymentToken) -> &mut Self {
            self.token = token;
            self
        }
        #[doc = "Sets the value of `token`."]
        pub fn with_token(mut self, token: DeploymentToken) -> Self {
            self.token = token;
            self
        }
        #[doc = "Sets the value of `fingerprint_script`."]
        pub fn set_fingerprint_script(&mut self, fingerprint_script: PathBuf) -> &mut Self {
            self.fingerprint_script = fingerprint_script;
            self
        }
        #[doc = "Sets the value of `fingerprint_script`."]
        pub fn with_fingerprint_script(mut self, fingerprint_script: PathBuf) -> Self {
            self.fingerprint_script = fingerprint_script;
            self
        }
        #[doc = "Sets the value of `ssl_cert`."]
        pub fn set_ssl_cert(&mut self, ssl_cert: ::std::option::Option<PathBuf>) -> &mut Self {
            self.ssl_cert = ssl_cert;
            self
        }
        #[doc = "Sets the value of `ssl_cert`."]
        pub fn with_ssl_cert(mut self, ssl_cert: ::std::option::Option<PathBuf>) -> Self {
            self.ssl_cert = ssl_cert;
            self
        }
        #[doc = "Sets the value of `ssl_key`."]
        pub fn set_ssl_key(&mut self, ssl_key: ::std::option::Option<PathBuf>) -> &mut Self {
            self.ssl_key = ssl_key;
            self
        }
        #[doc = "Sets the value of `ssl_key`."]
        pub fn with_ssl_key(mut self, ssl_key: ::std::option::Option<PathBuf>) -> Self {
            self.ssl_key = ssl_key;
            self
        }
        #[doc = "Sets the value of `dangerous_disable_tls`."]
        pub fn set_dangerous_disable_tls(
            &mut self,
            dangerous_disable_tls: ::std::option::Option<bool>,
        ) -> &mut Self {
            self.dangerous_disable_tls = dangerous_disable_tls;
            self
        }
        #[doc = "Sets the value of `dangerous_disable_tls`."]
        pub fn with_dangerous_disable_tls(
            mut self,
            dangerous_disable_tls: ::std::option::Option<bool>,
        ) -> Self {
            self.dangerous_disable_tls = dangerous_disable_tls;
            self
        }
        #[doc = "Sets the value of `exports`."]
        pub fn set_exports(
            &mut self,
            exports: ::std::option::Option<::std::vec::Vec<ExportConfig>>,
        ) -> &mut Self {
            self.exports = exports;
            self
        }
        #[doc = "Sets the value of `exports`."]
        pub fn with_exports(
            mut self,
            exports: ::std::option::Option<::std::vec::Vec<ExportConfig>>,
        ) -> Self {
            self.exports = exports;
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
                __sidex_serde::ser::RecordSerializer::new(__serializer, "Config", 7usize)?;
            __record.serialize_field("hub-url", &self.hub_url)?;
            __record.serialize_field("token", &self.token)?;
            __record.serialize_field("fingerprint-script", &self.fingerprint_script)?;
            __record.serialize_optional_field(
                "ssl-cert",
                ::core::option::Option::as_ref(&self.ssl_cert),
            )?;
            __record.serialize_optional_field(
                "ssl-key",
                ::core::option::Option::as_ref(&self.ssl_key),
            )?;
            __record.serialize_optional_field(
                "dangerous-disable-tls",
                ::core::option::Option::as_ref(&self.dangerous_disable_tls),
            )?;
            __record.serialize_optional_field(
                "exports",
                ::core::option::Option::as_ref(&self.exports),
            )?;
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
                                __serde::de::Error::invalid_length(0usize, &"record with 7 fields"),
                            );
                        }
                    };
                    let __field1 = match __serde::de::SeqAccess::next_element::<DeploymentToken>(
                        &mut __seq,
                    )? {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                __serde::de::Error::invalid_length(1usize, &"record with 7 fields"),
                            );
                        }
                    };
                    let __field2 =
                        match __serde::de::SeqAccess::next_element::<PathBuf>(&mut __seq)? {
                            ::core::option::Option::Some(__value) => __value,
                            ::core::option::Option::None => {
                                return ::core::result::Result::Err(
                                    __serde::de::Error::invalid_length(
                                        2usize,
                                        &"record with 7 fields",
                                    ),
                                );
                            }
                        };
                    let __field3 = match __serde::de::SeqAccess::next_element::<
                        ::std::option::Option<PathBuf>,
                    >(&mut __seq)?
                    {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                __serde::de::Error::invalid_length(3usize, &"record with 7 fields"),
                            );
                        }
                    };
                    let __field4 = match __serde::de::SeqAccess::next_element::<
                        ::std::option::Option<PathBuf>,
                    >(&mut __seq)?
                    {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                __serde::de::Error::invalid_length(4usize, &"record with 7 fields"),
                            );
                        }
                    };
                    let __field5 = match __serde::de::SeqAccess::next_element::<
                        ::std::option::Option<bool>,
                    >(&mut __seq)?
                    {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                __serde::de::Error::invalid_length(5usize, &"record with 7 fields"),
                            );
                        }
                    };
                    let __field6 = match __serde::de::SeqAccess::next_element::<
                        ::std::option::Option<::std::vec::Vec<ExportConfig>>,
                    >(&mut __seq)?
                    {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                __serde::de::Error::invalid_length(6usize, &"record with 7 fields"),
                            );
                        }
                    };
                    ::core::result::Result::Ok(Config {
                        hub_url: __field0,
                        token: __field1,
                        fingerprint_script: __field2,
                        ssl_cert: __field3,
                        ssl_key: __field4,
                        dangerous_disable_tls: __field5,
                        exports: __field6,
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
                    const __IDENTIFIERS: &'static [&'static str] = &[
                        "hub-url",
                        "token",
                        "fingerprint-script",
                        "ssl-cert",
                        "ssl-key",
                        "dangerous-disable-tls",
                        "exports",
                    ];
                    #[doc(hidden)]
                    const __EXPECTING_IDENTIFIERS: &'static str = "an identifier in [\"hub-url\", \"token\", \"fingerprint-script\", \"ssl-cert\", \"ssl-key\", \"dangerous-disable-tls\", \"exports\"]";
                    #[derive(:: core :: clone :: Clone, :: core :: marker :: Copy)]
                    #[doc(hidden)]
                    enum __Identifier {
                        __Identifier0,
                        __Identifier1,
                        __Identifier2,
                        __Identifier3,
                        __Identifier4,
                        __Identifier5,
                        __Identifier6,
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
                                2u64 => ::core::result::Result::Ok(__Identifier::__Identifier2),
                                3u64 => ::core::result::Result::Ok(__Identifier::__Identifier3),
                                4u64 => ::core::result::Result::Ok(__Identifier::__Identifier4),
                                5u64 => ::core::result::Result::Ok(__Identifier::__Identifier5),
                                6u64 => ::core::result::Result::Ok(__Identifier::__Identifier6),
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
                                "fingerprint-script" => {
                                    ::core::result::Result::Ok(__Identifier::__Identifier2)
                                }
                                "ssl-cert" => {
                                    ::core::result::Result::Ok(__Identifier::__Identifier3)
                                }
                                "ssl-key" => {
                                    ::core::result::Result::Ok(__Identifier::__Identifier4)
                                }
                                "dangerous-disable-tls" => {
                                    ::core::result::Result::Ok(__Identifier::__Identifier5)
                                }
                                "exports" => {
                                    ::core::result::Result::Ok(__Identifier::__Identifier6)
                                }
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
                                b"fingerprint-script" => {
                                    ::core::result::Result::Ok(__Identifier::__Identifier2)
                                }
                                b"ssl-cert" => {
                                    ::core::result::Result::Ok(__Identifier::__Identifier3)
                                }
                                b"ssl-key" => {
                                    ::core::result::Result::Ok(__Identifier::__Identifier4)
                                }
                                b"dangerous-disable-tls" => {
                                    ::core::result::Result::Ok(__Identifier::__Identifier5)
                                }
                                b"exports" => {
                                    ::core::result::Result::Ok(__Identifier::__Identifier6)
                                }
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
                    let mut __field1: ::core::option::Option<DeploymentToken> =
                        ::core::option::Option::None;
                    let mut __field2: ::core::option::Option<PathBuf> =
                        ::core::option::Option::None;
                    let mut __field3: ::core::option::Option<::std::option::Option<PathBuf>> =
                        ::core::option::Option::None;
                    let mut __field4: ::core::option::Option<::std::option::Option<PathBuf>> =
                        ::core::option::Option::None;
                    let mut __field5: ::core::option::Option<::std::option::Option<bool>> =
                        ::core::option::Option::None;
                    let mut __field6: ::core::option::Option<
                        ::std::option::Option<::std::vec::Vec<ExportConfig>>,
                    > = ::core::option::Option::None;
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
                                    __serde::de::MapAccess::next_value::<DeploymentToken>(
                                        &mut __map,
                                    )?,
                                );
                            }
                            __Identifier::__Identifier2 => {
                                if ::core::option::Option::is_some(&__field2) {
                                    return ::core::result::Result::Err(
                                        <__A::Error as __serde::de::Error>::duplicate_field(
                                            "fingerprint-script",
                                        ),
                                    );
                                }
                                __field2 = ::core::option::Option::Some(
                                    __serde::de::MapAccess::next_value::<PathBuf>(&mut __map)?,
                                );
                            }
                            __Identifier::__Identifier3 => {
                                if ::core::option::Option::is_some(&__field3) {
                                    return ::core::result::Result::Err(
                                        <__A::Error as __serde::de::Error>::duplicate_field(
                                            "ssl-cert",
                                        ),
                                    );
                                }
                                __field3 = ::core::option::Option::Some(
                                    __serde::de::MapAccess::next_value::<
                                        ::std::option::Option<PathBuf>,
                                    >(&mut __map)?,
                                );
                            }
                            __Identifier::__Identifier4 => {
                                if ::core::option::Option::is_some(&__field4) {
                                    return ::core::result::Result::Err(
                                        <__A::Error as __serde::de::Error>::duplicate_field(
                                            "ssl-key",
                                        ),
                                    );
                                }
                                __field4 = ::core::option::Option::Some(
                                    __serde::de::MapAccess::next_value::<
                                        ::std::option::Option<PathBuf>,
                                    >(&mut __map)?,
                                );
                            }
                            __Identifier::__Identifier5 => {
                                if ::core::option::Option::is_some(&__field5) {
                                    return ::core::result::Result::Err(
                                        <__A::Error as __serde::de::Error>::duplicate_field(
                                            "dangerous-disable-tls",
                                        ),
                                    );
                                }
                                __field5 = ::core::option::Option::Some(
                                    __serde::de::MapAccess::next_value::<
                                        ::std::option::Option<bool>,
                                    >(&mut __map)?,
                                );
                            }
                            __Identifier::__Identifier6 => {
                                if ::core::option::Option::is_some(&__field6) {
                                    return ::core::result::Result::Err(
                                        <__A::Error as __serde::de::Error>::duplicate_field(
                                            "exports",
                                        ),
                                    );
                                }
                                __field6 = ::core::option::Option::Some(
                                    __serde::de::MapAccess::next_value::<
                                        ::std::option::Option<::std::vec::Vec<ExportConfig>>,
                                    >(&mut __map)?,
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
                    let __field2 = match __field2 {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                <__A::Error as __serde::de::Error>::missing_field(
                                    "fingerprint-script",
                                ),
                            );
                        }
                    };
                    let __field3 = match __field3 {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => ::core::option::Option::None,
                    };
                    let __field4 = match __field4 {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => ::core::option::Option::None,
                    };
                    let __field5 = match __field5 {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => ::core::option::Option::None,
                    };
                    let __field6 = match __field6 {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => ::core::option::Option::None,
                    };
                    ::core::result::Result::Ok(Config {
                        hub_url: __field0,
                        token: __field1,
                        fingerprint_script: __field2,
                        ssl_cert: __field3,
                        ssl_key: __field4,
                        dangerous_disable_tls: __field5,
                        exports: __field6,
                    })
                }
            }
            #[doc(hidden)]
            const __FIELDS: &'static [&'static str] = &[
                "hub-url",
                "token",
                "fingerprint-script",
                "ssl-cert",
                "ssl-key",
                "dangerous-disable-tls",
                "exports",
            ];
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
    #[doc = "Service export configuration.\n"]
    #[derive(Clone, Debug)]
    pub enum ExportConfig {
        #[doc = "HTTP export configuration.\n"]
        Http(HttpExportConfig),
    }
    #[automatically_derived]
    impl __serde::Serialize for ExportConfig {
        fn serialize<__S: __serde::Serializer>(
            &self,
            __serializer: __S,
        ) -> ::std::result::Result<__S::Ok, __S::Error> {
            let __serializer =
                __sidex_serde::ser::VariantSerializer::new(__serializer, "ExportConfig");
            match self {
                Self::Http(__value) => {
                    __serializer.serialize_internally_tagged("protocol", "http", 0u32, __value)
                }
            }
        }
    }
    #[automatically_derived]
    impl<'de> __serde::Deserialize<'de> for ExportConfig {
        fn deserialize<__D: __serde::Deserializer<'de>>(
            __deserializer: __D,
        ) -> ::std::result::Result<Self, __D::Error> {
            #[doc(hidden)]
            const __IDENTIFIERS: &'static [&'static str] = &["http"];
            #[doc(hidden)]
            const __EXPECTING_IDENTIFIERS: &'static str = "an identifier in [\"http\"]";
            #[derive(:: core :: clone :: Clone, :: core :: marker :: Copy)]
            #[doc(hidden)]
            enum __Identifier {
                __Identifier0,
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
                fn visit_u64<__E>(self, __value: u64) -> ::core::result::Result<Self::Value, __E>
                where
                    __E: __serde::de::Error,
                {
                    match __value {
                        0u64 => ::core::result::Result::Ok(__Identifier::__Identifier0),
                        __variant => {
                            ::core::result::Result::Err(__serde::de::Error::invalid_value(
                                __serde::de::Unexpected::Unsigned(__variant),
                                &__EXPECTING_IDENTIFIERS,
                            ))
                        }
                    }
                }
                fn visit_str<__E>(self, __value: &str) -> ::core::result::Result<Self::Value, __E>
                where
                    __E: __serde::de::Error,
                {
                    match __value {
                        "http" => ::core::result::Result::Ok(__Identifier::__Identifier0),
                        __variant => ::core::result::Result::Err(
                            __serde::de::Error::unknown_variant(__variant, __IDENTIFIERS),
                        ),
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
                        b"http" => ::core::result::Result::Ok(__Identifier::__Identifier0),
                        __variant => {
                            ::core::result::Result::Err(__serde::de::Error::invalid_value(
                                __serde::de::Unexpected::Bytes(__variant),
                                &__EXPECTING_IDENTIFIERS,
                            ))
                        }
                    }
                }
            }
            impl<'de> __serde::Deserialize<'de> for __Identifier {
                #[inline]
                fn deserialize<__D>(__deserializer: __D) -> ::core::result::Result<Self, __D::Error>
                where
                    __D: __serde::Deserializer<'de>,
                {
                    __serde::Deserializer::deserialize_identifier(
                        __deserializer,
                        __IdentifierVisitor,
                    )
                }
            }
            #[doc(hidden)]
            const __VARIANTS: &'static [&'static str] = &["http"];
            if __serde::Deserializer::is_human_readable(&__deserializer) {
                let __tagged = __sidex_serde::de::tagged::deserialize_tagged_variant::<
                    __Identifier,
                    __D,
                >(__deserializer, "protocol")?;
                match __tagged.tag {
                    __Identifier::__Identifier0 => ::core::result::Result::Ok(ExportConfig::Http(
                        __tagged.deserialize_internally_tagged::<HttpExportConfig, __D::Error>()?,
                    )),
                }
            } else {
                #[doc(hidden)]
                struct __Visitor {
                    __phantom_vars: ::core::marker::PhantomData<fn(&())>,
                }
                impl<'de> __serde::de::Visitor<'de> for __Visitor {
                    type Value = ExportConfig;
                    fn expecting(
                        &self,
                        __formatter: &mut ::core::fmt::Formatter,
                    ) -> ::core::fmt::Result {
                        ::core::fmt::Formatter::write_str(__formatter, "enum ExportConfig")
                    }
                    #[inline]
                    fn visit_str<__E>(
                        self,
                        __value: &str,
                    ) -> ::core::result::Result<Self::Value, __E>
                    where
                        __E: __serde::de::Error,
                    {
                        let __identifier = __IdentifierVisitor.visit_str(__value)?;
                        #[allow(unreachable_patterns)]
                        match __identifier {
                            _ => Err(__E::invalid_value(
                                __serde::de::Unexpected::Str(__value),
                                &self,
                            )),
                        }
                    }
                    #[inline]
                    fn visit_enum<__A>(
                        self,
                        __data: __A,
                    ) -> ::core::result::Result<Self::Value, __A::Error>
                    where
                        __A: __serde::de::EnumAccess<'de>,
                    {
                        match __serde::de::EnumAccess::variant::<__Identifier>(__data)? {
                            (__Identifier::__Identifier0, __variant) => {
                                let __value = __serde::de::VariantAccess::newtype_variant::<
                                    HttpExportConfig,
                                >(__variant)?;
                                ::core::result::Result::Ok(ExportConfig::Http(__value))
                            }
                        }
                    }
                }
                __serde::Deserializer::deserialize_enum(
                    __deserializer,
                    "ExportConfig",
                    __VARIANTS,
                    __Visitor {
                        __phantom_vars: ::core::marker::PhantomData,
                    },
                )
            }
        }
    }
    #[doc = "HTTP export configuration.\n"]
    #[derive(Clone, Debug)]
    pub struct HttpExportConfig {
        #[doc = "Name of the export.\n"]
        pub name: ::std::string::String,
        #[doc = "Port the service listens on.\n"]
        pub port: u16,
        #[doc = "URL path prefix for the service.\n"]
        pub path: ::std::option::Option<::std::string::String>,
    }
    impl HttpExportConfig {
        #[doc = "Creates a new [`HttpExportConfig`]."]
        pub fn new(name: ::std::string::String, port: u16) -> Self {
            Self {
                name,
                port,
                path: ::std::default::Default::default(),
            }
        }
        #[doc = "Sets the value of `name`."]
        pub fn set_name(&mut self, name: ::std::string::String) -> &mut Self {
            self.name = name;
            self
        }
        #[doc = "Sets the value of `name`."]
        pub fn with_name(mut self, name: ::std::string::String) -> Self {
            self.name = name;
            self
        }
        #[doc = "Sets the value of `port`."]
        pub fn set_port(&mut self, port: u16) -> &mut Self {
            self.port = port;
            self
        }
        #[doc = "Sets the value of `port`."]
        pub fn with_port(mut self, port: u16) -> Self {
            self.port = port;
            self
        }
        #[doc = "Sets the value of `path`."]
        pub fn set_path(
            &mut self,
            path: ::std::option::Option<::std::string::String>,
        ) -> &mut Self {
            self.path = path;
            self
        }
        #[doc = "Sets the value of `path`."]
        pub fn with_path(mut self, path: ::std::option::Option<::std::string::String>) -> Self {
            self.path = path;
            self
        }
    }
    #[automatically_derived]
    impl __serde::Serialize for HttpExportConfig {
        fn serialize<__S: __serde::Serializer>(
            &self,
            __serializer: __S,
        ) -> ::std::result::Result<__S::Ok, __S::Error> {
            let mut __record = __sidex_serde::ser::RecordSerializer::new(
                __serializer,
                "HttpExportConfig",
                3usize,
            )?;
            __record.serialize_field("name", &self.name)?;
            __record.serialize_field("port", &self.port)?;
            __record
                .serialize_optional_field("path", ::core::option::Option::as_ref(&self.path))?;
            __record.end()
        }
    }
    #[automatically_derived]
    impl<'de> __serde::Deserialize<'de> for HttpExportConfig {
        fn deserialize<__D: __serde::Deserializer<'de>>(
            __deserializer: __D,
        ) -> ::std::result::Result<Self, __D::Error> {
            #[doc(hidden)]
            struct __Visitor {
                __phantom_vars: ::core::marker::PhantomData<fn(&())>,
            }
            impl<'de> __serde::de::Visitor<'de> for __Visitor {
                type Value = HttpExportConfig;
                fn expecting(
                    &self,
                    __formatter: &mut ::core::fmt::Formatter,
                ) -> ::core::fmt::Result {
                    ::core::fmt::Formatter::write_str(__formatter, "record HttpExportConfig")
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
                                __serde::de::Error::invalid_length(0usize, &"record with 3 fields"),
                            );
                        }
                    };
                    let __field1 = match __serde::de::SeqAccess::next_element::<u16>(&mut __seq)? {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                __serde::de::Error::invalid_length(1usize, &"record with 3 fields"),
                            );
                        }
                    };
                    let __field2 = match __serde::de::SeqAccess::next_element::<
                        ::std::option::Option<::std::string::String>,
                    >(&mut __seq)?
                    {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                __serde::de::Error::invalid_length(2usize, &"record with 3 fields"),
                            );
                        }
                    };
                    ::core::result::Result::Ok(HttpExportConfig {
                        name: __field0,
                        port: __field1,
                        path: __field2,
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
                    const __IDENTIFIERS: &'static [&'static str] = &["name", "port", "path"];
                    #[doc(hidden)]
                    const __EXPECTING_IDENTIFIERS: &'static str =
                        "an identifier in [\"name\", \"port\", \"path\"]";
                    #[derive(:: core :: clone :: Clone, :: core :: marker :: Copy)]
                    #[doc(hidden)]
                    enum __Identifier {
                        __Identifier0,
                        __Identifier1,
                        __Identifier2,
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
                                2u64 => ::core::result::Result::Ok(__Identifier::__Identifier2),
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
                                "name" => ::core::result::Result::Ok(__Identifier::__Identifier0),
                                "port" => ::core::result::Result::Ok(__Identifier::__Identifier1),
                                "path" => ::core::result::Result::Ok(__Identifier::__Identifier2),
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
                                b"name" => ::core::result::Result::Ok(__Identifier::__Identifier0),
                                b"port" => ::core::result::Result::Ok(__Identifier::__Identifier1),
                                b"path" => ::core::result::Result::Ok(__Identifier::__Identifier2),
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
                    let mut __field1: ::core::option::Option<u16> = ::core::option::Option::None;
                    let mut __field2: ::core::option::Option<
                        ::std::option::Option<::std::string::String>,
                    > = ::core::option::Option::None;
                    while let ::core::option::Option::Some(__key) =
                        __serde::de::MapAccess::next_key::<__Identifier>(&mut __map)?
                    {
                        match __key {
                            __Identifier::__Identifier0 => {
                                if ::core::option::Option::is_some(&__field0) {
                                    return ::core::result::Result::Err(
                                        <__A::Error as __serde::de::Error>::duplicate_field("name"),
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
                                        <__A::Error as __serde::de::Error>::duplicate_field("port"),
                                    );
                                }
                                __field1 = ::core::option::Option::Some(
                                    __serde::de::MapAccess::next_value::<u16>(&mut __map)?,
                                );
                            }
                            __Identifier::__Identifier2 => {
                                if ::core::option::Option::is_some(&__field2) {
                                    return ::core::result::Result::Err(
                                        <__A::Error as __serde::de::Error>::duplicate_field("path"),
                                    );
                                }
                                __field2 = ::core::option::Option::Some(
                                    __serde::de::MapAccess::next_value::<
                                        ::std::option::Option<::std::string::String>,
                                    >(&mut __map)?,
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
                                <__A::Error as __serde::de::Error>::missing_field("name"),
                            );
                        }
                    };
                    let __field1 = match __field1 {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => {
                            return ::core::result::Result::Err(
                                <__A::Error as __serde::de::Error>::missing_field("port"),
                            );
                        }
                    };
                    let __field2 = match __field2 {
                        ::core::option::Option::Some(__value) => __value,
                        ::core::option::Option::None => ::core::option::Option::None,
                    };
                    ::core::result::Result::Ok(HttpExportConfig {
                        name: __field0,
                        port: __field1,
                        path: __field2,
                    })
                }
            }
            #[doc(hidden)]
            const __FIELDS: &'static [&'static str] = &["name", "port", "path"];
            __serde::Deserializer::deserialize_struct(
                __deserializer,
                "HttpExportConfig",
                __FIELDS,
                __Visitor {
                    __phantom_vars: ::core::marker::PhantomData,
                },
            )
        }
    }
}

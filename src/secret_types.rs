macro_rules! one_of {
  ($lit:literal $(,)?) => {
    concat!("'", $lit, "'")
  };
  ($first:literal, $second:literal $(,)?) => {
    concat!("either '", $first, "', or '", $second, "'")
  };
  (
    $first:literal, $($lit:literal),+$(,)?
  ) => {
    one_of!(@acc [$($lit)+] ["one of '" $first "'"])
  };
  (@acc [$last:literal] [$($acc:literal)+]) => {
    concat!($($acc,)+ ", or '", $last, "'")
  };
  (@acc [$next:literal $($lit:literal)+] [$($acc:literal)+]) => {
    one_of!(@acc [$($lit)+] [$($acc)+ ", '" $next "'"])
  };
}

macro_rules! str_enum {
  (
    $(#[$m:meta])*
    $vis:vis enum $name:ident {
      $(
        $(#[$var_m:meta])*
        $var_name:ident = $var_val:literal
      ),+$(,)?
    }
  ) => {
    $(#[$m])*
    $vis enum $name {
      $(
        $(#[$var_m])*
        $var_name,
      )+
    }

    impl ::core::fmt::Display for $name {
      fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        match self {
          $(
            Self::$var_name => f.write_str($var_val),
          )+
        }
      }
    }

    impl<'de> ::serde::Deserialize<'de> for $name {
      fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
      where
        D: ::serde::Deserializer<'de>,
      {
        struct Visitor;
        impl<'de> ::serde::de::Visitor<'de> for Visitor {
          type Value = $name;

          fn expecting(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
            f.write_str(one_of!($($var_val,)*))
          }

          fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
          where
            E: ::serde::de::Error,
          {
            match v {
              $(
                $var_val => Ok($name::$var_name),
              )*
              _ => Err(E::invalid_value(::serde::de::Unexpected::Str(v), &self)),
            }
          }
        }

        deserializer.deserialize_str(Visitor)
      }
    }

    impl ::serde::Serialize for $name {
      fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
      where
        S: ::serde::Serializer,
      {
        match self {
          $(
            Self::$var_name => $var_val.serialize(serializer),
          )+
        }
      }
    }

    impl TryFrom<&str> for $name {
      type Error = ();

      fn try_from(value: &str) -> Result<Self, ()> {
        match value {
          $(
            $var_val => Ok(Self::$var_name),
          )*
          _ => Err(()),
        }
      }
    }

    impl schemars::JsonSchema for $name {
      fn schema_name() -> String {
        stringify!($name).into()
      }

      fn json_schema(_: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        schemars::schema::Schema::Object(schemars::schema::SchemaObject {
          instance_type: Some(schemars::schema::InstanceType::String.into()),
          enum_values: Some(vec![
            $(serde_json::Value::from($var_val),)*
          ]),
          ..Default::default()
        })
      }
    }
  };
}

str_enum! {
  #[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
  pub enum AutoSecretType {
    Uuid = "uuid",
    Ulid = "ulid",
  }
}

impl AutoSecretType {
  pub fn generate(&self) -> String {
    match self {
      AutoSecretType::Uuid => uuid::Uuid::new_v4().to_string(),
      AutoSecretType::Ulid => ulid::Ulid::new().to_string(),
    }
  }
}

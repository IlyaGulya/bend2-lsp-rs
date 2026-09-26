# Fixed source corpus for the source-analysis performance gate.
import Base.List

type Maybe:
  None
  Some(value)

def identity(value):
  value

def apply_twice(function, value):
  function(function(value))

def map_maybe(function, maybe):
  match maybe:
    case None: None
    case Some(value): Some(function(value))

def fold_values(function, state, values):
  match values:
    case Nil: state
    case Cons(head, tail): fold_values(function, function(state, head), tail)

def transform_00(value):
  identity(value)
def transform_01(value):
  apply_twice(identity, value)
def transform_02(value):
  map_maybe(identity, value)
def transform_03(value):
  fold_values(identity, value, Nil)
def transform_04(value):
  identity(transform_00(value))
def transform_05(value):
  identity(transform_01(value))
def transform_06(value):
  identity(transform_02(value))
def transform_07(value):
  identity(transform_03(value))
def transform_08(value):
  apply_twice(identity, transform_00(value))
def transform_09(value):
  apply_twice(identity, transform_01(value))
def transform_10(value):
  apply_twice(identity, transform_02(value))
def transform_11(value):
  apply_twice(identity, transform_03(value))
def transform_12(value):
  map_maybe(identity, transform_00(value))
def transform_13(value):
  map_maybe(identity, transform_01(value))
def transform_14(value):
  map_maybe(identity, transform_02(value))
def transform_15(value):
  map_maybe(identity, transform_03(value))
def transform_16(value):
  fold_values(identity, transform_00(value), Nil)
def transform_17(value):
  fold_values(identity, transform_01(value), Nil)
def transform_18(value):
  fold_values(identity, transform_02(value), Nil)
def transform_19(value):
  fold_values(identity, transform_03(value), Nil)
def transform_20(value):
  identity(transform_04(value))
def transform_21(value):
  identity(transform_05(value))
def transform_22(value):
  identity(transform_06(value))
def transform_23(value):
  identity(transform_07(value))
def transform_24(value):
  apply_twice(identity, transform_08(value))
def transform_25(value):
  apply_twice(identity, transform_09(value))
def transform_26(value):
  apply_twice(identity, transform_10(value))
def transform_27(value):
  apply_twice(identity, transform_11(value))
def transform_28(value):
  map_maybe(identity, transform_12(value))
def transform_29(value):
  map_maybe(identity, transform_13(value))
def transform_30(value):
  map_maybe(identity, transform_14(value))
def transform_31(value):
  map_maybe(identity, transform_15(value))

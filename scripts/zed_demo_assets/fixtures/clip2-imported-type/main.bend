import Base
import dep.bend as Dep

def f(n: Nat, acc: U32) -> U32:
  match n:
    case 0n:
      acc
    case 1n+m:
      xs = Dep.build(3n, [])
      f(m, acc)

def g(+x: Nat, y: Nat) -> Nat:
    Nat.add(x, y)

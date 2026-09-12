#define S(x) #x
#define B(x) S(a x ## +b)
#define P(x,y) S((x ## y))
#define Q(x,y,z) S(a x ## y ## z)
#define V(...) S(a __VA_ARGS__ ## +b)
b B()
p P(,y)
q Q(,,z)
r Q(x,,z)
v V()
#define C2(x) a x ## "s"
#define D(x,y) f(x ## y)
#define N(args...) S(a args ## +b)
c C2()
d D(,y)
n N()
e B(
)
#define CALL(o, arg, ...) g(o, arg ## __VA_ARGS__)
#define LOGV(t, fmt, args...) h(t, fmt ## args)
x CALL(o,)
y CALL(o,,)
z LOGV(t,)
w CALL(o,
)
u LOGV(t,
)
#define GNU(o, ...) g(o, ## __VA_ARGS__)
m GNU(o)
k GNU(o,
)

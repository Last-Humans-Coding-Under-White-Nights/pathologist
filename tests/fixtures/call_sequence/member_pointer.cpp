struct C {};
using M = void (C::*)();
C *object();
C &reference();
int index();
M callbacks[1];
void arrow() { (object()->*callbacks[index()])(); }
void dot() { (reference().*callbacks[index()])(); }

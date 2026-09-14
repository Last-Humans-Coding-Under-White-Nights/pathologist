// Two overloads of one arity, only one with a default: the other does not
// take one argument.
void Format(int value, int width = 0) { (void)value; (void)width; }
void Format(void (*first)(), void (*second)()) { first(); second(); }

void FormatUnknown() { Format(NotDeclaredEither()); }

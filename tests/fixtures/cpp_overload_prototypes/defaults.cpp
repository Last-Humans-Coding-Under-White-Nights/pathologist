#include "defaults.hpp"

using namespace std;

namespace text {
void Trim(const string &s, char c) { (void)s; (void)c; }
}

namespace text {
void TrimOne(const string &s) { Trim(s); }
}

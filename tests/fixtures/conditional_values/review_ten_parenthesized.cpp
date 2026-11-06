using TenF=void(*)();
void alpha(); void beta(); void unrelated();
TenF ten_paren_cell=beta, ten_paren_override_cell=beta;
struct TenParen { int i; TenF f=((ten_paren_cell=alpha),beta); };
struct TenParenOverride { int i; TenF f=((ten_paren_override_cell=unrelated),beta); };
TenParen ten_paren(1);
TenParenOverride ten_paren_override(1,alpha);
void ten_parenthesized_default() { ten_paren_cell(); }
void ten_parenthesized_override() { ten_paren_override_cell(); }

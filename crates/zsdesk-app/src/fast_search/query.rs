//! Filename query grammar shared by scope/type filters and native search.
use anyhow::{Result,ensure,bail};
#[derive(Debug)]enum Expr{Name(String),Path(String),Ext(Vec<String>),Kind(bool),And(Vec<Expr>),Or(Vec<Expr>),Not(Box<Expr>)}
#[derive(Clone,Debug,PartialEq)]enum Token{Word(String),Open,Close,Or,Not}
#[derive(Clone,Copy,Debug,PartialEq)]pub enum Probe{No,Yes,Path}
pub struct Query{expr:Expr,pinyin:bool}
impl Query{
    pub fn parse(text:&str,pinyin:bool)->Result<Self>{
        ensure!(text.chars().count()<=512&&!text.contains('\0'),"搜索内容过长或无效");let mut tokens=Vec::new();let mut word=String::new();let mut quote=false;
        let flush=|tokens:&mut Vec<Token>,word:&mut String|{if !word.is_empty(){tokens.push(Token::Word(std::mem::take(word)));}};
        for ch in text.chars(){if ch=='"'{quote=!quote;continue;}if quote{word.push(ch);continue;}
            match ch{'<'=>{flush(&mut tokens,&mut word);tokens.push(Token::Open)},'>'=>{flush(&mut tokens,&mut word);tokens.push(Token::Close)},
                '|'=>{flush(&mut tokens,&mut word);tokens.push(Token::Or)},'!' if word.is_empty()=>tokens.push(Token::Not),
                c if c.is_whitespace()=>flush(&mut tokens,&mut word),_=>word.push(ch)}}
        ensure!(!quote,"搜索引号未闭合");flush(&mut tokens,&mut word);ensure!(!tokens.is_empty()&&tokens.len()<=256,"请输入搜索关键词");
        let mut parser=Parser{tokens,at:0};let expr=parser.or(0)?;ensure!(parser.at==parser.tokens.len(),"搜索分组不完整");Ok(Self{expr,pinyin})
    }
    pub fn probe(&self,name:&str,dir:bool)->Probe{self.expr.probe(name,dir,self.pinyin)}
    pub fn matches(&self,name:&str,dir:bool,path:&str)->bool{self.expr.matches(name,dir,path,self.pinyin)}
}
struct Parser{tokens:Vec<Token>,at:usize}
impl Parser{
    fn or(&mut self,depth:usize)->Result<Expr>{ensure!(depth<32,"搜索分组过深");let mut values=vec![self.and(depth+1)?];
        while self.tokens.get(self.at)==Some(&Token::Or){self.at+=1;values.push(self.and(depth+1)?);}Ok(if values.len()==1{values.pop().unwrap()}else{Expr::Or(values)})}
    fn and(&mut self,depth:usize)->Result<Expr>{let mut values=Vec::new();while !matches!(self.tokens.get(self.at),None|Some(Token::Close|Token::Or)){values.push(self.atom(depth+1)?);}
        ensure!(!values.is_empty(),"搜索运算符缺少关键词");Ok(if values.len()==1{values.pop().unwrap()}else{Expr::And(values)})}
    fn atom(&mut self,depth:usize)->Result<Expr>{ensure!(depth<32,"搜索分组过深");let token=self.tokens.get(self.at).cloned();self.at+=1;
        match token{Some(Token::Not)=>Ok(Expr::Not(Box::new(self.atom(depth+1)?))),Some(Token::Open)=>{let value=self.or(depth+1)?;ensure!(self.tokens.get(self.at)==Some(&Token::Close),"搜索分组未闭合");self.at+=1;Ok(value)},
            Some(Token::Word(text))=>{let text=text.to_lowercase().replace('/',"\\");
                if text=="folder:"{return Ok(Expr::Kind(true));}if text=="file:"{return Ok(Expr::Kind(false));}
                if let Some(value)=text.strip_prefix("ext:"){let values:Vec<_>=value.split(';').filter(|v|!v.is_empty()).map(|v|v.trim_start_matches('.').to_owned()).collect();ensure!(!values.is_empty(),"文件类型筛选为空");return Ok(Expr::Ext(values));}
                if let Some(value)=text.strip_prefix("path:"){ensure!(!value.is_empty(),"路径筛选为空");return Ok(Expr::Path(value.into()));}
                if text.contains('\\'){return Ok(Expr::Path(text));}
                if text.contains(':'){bail!("自带搜索支持文件名、通配符、分组、排除、ext:、file:、folder: 和 path:；此语法可切换 Everything 使用");}
                Ok(Expr::Name(text))},_=>bail!("搜索表达式无效")}
    }
}
impl Expr{
    fn probe(&self,name:&str,dir:bool,pinyin:bool)->Probe{
        match self{
            Self::Path(_)=>Probe::Path,
            Self::And(values)=>{let mut path=false;for value in values{match value.probe(name,dir,pinyin){Probe::No=>return Probe::No,Probe::Path=>path=true,_=>{}}}if path{Probe::Path}else{Probe::Yes}},
            Self::Or(values)=>{let mut path=false;for value in values{match value.probe(name,dir,pinyin){Probe::Yes=>return Probe::Yes,Probe::Path=>path=true,_=>{}}}if path{Probe::Path}else{Probe::No}},
            Self::Not(value)=>match value.probe(name,dir,pinyin){Probe::No=>Probe::Yes,Probe::Yes=>Probe::No,Probe::Path=>Probe::Path},
            _=>if self.matches(name,dir,"",pinyin){Probe::Yes}else{Probe::No},
        }
    }
    fn matches(&self,name:&str,dir:bool,path:&str,pinyin:bool)->bool{
        match self{
            Self::Name(value)=>pattern(value,name)||(pinyin&&initials_match(value,name)),Self::Path(value)=>pattern(value,path),
            Self::Ext(extensions)=>!dir&&name.rsplit_once('.').is_some_and(|(_,ext)|extensions.iter().any(|value|value==ext)),
            Self::Kind(expected)=>dir==*expected,Self::And(values)=>values.iter().all(|v|v.matches(name,dir,path,pinyin)),
            Self::Or(values)=>values.iter().any(|v|v.matches(name,dir,path,pinyin)),Self::Not(value)=>!value.matches(name,dir,path,pinyin),
        }
    }
}
fn pattern(pattern:&str,text:&str)->bool{
    if pattern=="*"{return true;}
    if !pattern.contains(['*','?']){return text.contains(pattern);}
    let pattern:Vec<_>=pattern.chars().collect();let text:Vec<_>=text.chars().collect();let(mut p,mut t,mut star,mut retry)=(0,0,None,0);
    while t<text.len(){if p<pattern.len()&&(pattern[p]=='?'||pattern[p]==text[t]){p+=1;t+=1;}
        else if p<pattern.len()&&pattern[p]=='*'{star=Some(p);p+=1;retry=t;}
        else if let Some(s)=star{retry+=1;t=retry;p=s+1;}else{return false;}}
    while p<pattern.len()&&pattern[p]=='*'{p+=1;}p==pattern.len()
}
fn initials_match(query:&str,name:&str)->bool{
    use pinyin::ToPinyin;
    if query.is_empty()||!query.bytes().all(|c|c.is_ascii_alphanumeric()){return false;}
    let initials:String=name.chars().map(|ch|ch.to_pinyin().map(|p|p.plain().as_bytes()[0]as char).unwrap_or(ch)).collect();
    initials.contains(query)
}
#[cfg(test)]mod tests{
    use super::*;
    #[test]fn type_scope_group_and_negation_do_not_escape(){
        let query=Query::parse(r#""e:\资料\" ext:pdf;docx <合同 | 图纸> !草稿"#,false).unwrap();
        assert!(query.matches("工程合同.pdf",false,r"e:\资料\工程合同.pdf"));
        assert!(!query.matches("工程合同.pdf",false,r"e:\其他\工程合同.pdf"));
        assert!(!query.matches("图纸草稿.docx",false,r"e:\资料\图纸草稿.docx"));
        assert!(!query.matches("合同.pdf",true,r"e:\资料\合同.pdf"));
    }
    #[test]fn unicode_wildcards_pinyin_and_invalid_syntax(){
        assert!(Query::parse("中文?.txt",false).unwrap().matches("中文甲.txt",false,""));
        assert!(Query::parse("zgr",true).unwrap().matches("中国人.txt",false,""));
        assert!(!Query::parse("zgr",false).unwrap().matches("中国人.txt",false,""));
        for text in ["<a","a |","regex:a","\"a","!","<>"]{assert!(Query::parse(text,false).is_err(),"{text}");}
    }
}

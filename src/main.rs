// main.rs

mod lexer;
mod ast;


fn main() {
    let source_code = r#"
    
    fun substract(a: int, b: int): int {
        const result = a - b * c;
        return result;
    }
    // This is a comment
    struct test {
        a: int,
        b: int,
    }
    
    "#;
    
    // fun ioasjdasiod(a: int, b:int): int {
    //     return sub(a * b, 1) - 100 + (5*6+4)
    // }
    
    // fun main() {
    //         const test = test { a: 1, b: 2 };
    //         const result = substract(test.a, test.b);
    //         var a = 1;
    //         var b = 2;
    //         const or = a || b;
    //         const and = a && b;
    //         const c = add(a, b);
    //     }
    
    // fun add(a: int, b: int): int {
    //     const result = a + b;
    //     return result;
    // }

   
    
    let lexer = lexer::Lexer::new(source_code);
    let tokens = lexer.analyse_source();
    println!("TOKENS: {:?} ", tokens);
    let ast = ast::create_ast(&tokens);

    // Now 'ast' contains your Abstract Syntax Tree
    println!("Lexer: {:#?}", tokens);
    println!("Ast: {:#?}", ast);
}

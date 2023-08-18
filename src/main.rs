// main.rs

mod lexer;
mod ast;


fn main() {
    let source_code = r#"
    struct test {
        a: int,
        b: int,
    }
    
    fun substract(a: int, b: int): int {
        const result = a - b;
        return result;
    }

    fun add(a: int, b: int): int {
        const result = a + b;
        return result;
    }

    fun main() {
        const test = test { a: 1, b: 2 };
        const result = substract(test.a, test.b);
        var a = 1;
        var b = 2;
        const or = a || b;
        const and = a && b;
        const c = add(a, b);
    }

    "#;

    let mut lexer = lexer::Lexer::new(source_code);
    let mut tokens = Vec::new();

    loop {
        let token = lexer.next_token();
        if token == lexer::Token::Eof {
            break;
        }

        tokens.push(token.clone());
    }


    let ast = create_ast(&tokens);

    // Now 'ast' contains your Abstract Syntax Tree
    println!("{:#?}", ast);
}

fn create_ast(tokens: &[lexer::Token]) -> Vec<ast::AstNode>{
    let mut ast = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        println!("index: {} {:?}", index, tokens[index]);
        match tokens[index] {
            lexer::Token::Struct => {
                // Create StructNode and append to ast
                let struct_node = ast::create_struct_ast(&mut index, &tokens);
                ast.push(ast::AstNode::Struct(struct_node));
            },
            lexer::Token::Fun => {
                // Check if Fun is Main
                if let lexer::Token::Main = tokens[index + 1] {
                    // Create MainNode and append to ast
                    let main_node = ast::create_main_ast(&mut index, &tokens);
                    ast.push(ast::AstNode::MainFunction(main_node));
                } else {
                    // Create FunctionNode and append to ast
                    let function_node = ast::create_function_ast(&mut index, &tokens);
                    ast.push(ast::AstNode::Function(function_node));
                }
            },
            lexer::Token::Main => {
                // Create MainNode and append to ast
                let main_node = ast::create_main_ast(&mut index, &tokens);
                ast.push(ast::AstNode::MainFunction(main_node));
            },
            _ => println!("Not implemented"),
        }
        index += 1;
    }
    return ast;
}
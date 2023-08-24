use crate::lexer;

// ast.rs
#[derive(Debug)]
pub enum AstNode {
    Struct(StructNode),
    Function(FunctionNode),
    MainFunction(MainFunctionNode),
    // Add other AST node types as needed
}

#[derive(Debug)]
pub struct StructNode {
    pub name: String,
    pub fields: Vec<FieldNode>,
}

#[derive(Debug)]
pub struct FieldNode {
    pub name: String,
    pub data_type: TypeNode,
}

#[derive(Debug)]
pub struct FunctionNode {
    pub name: String,
    pub parameters: Vec<ParameterNode>,
    pub return_type: TypeNode,
    pub body: Vec<StatementNode>,
}
#[derive(Debug)]
pub struct MainFunctionNode {
    pub function: FunctionNode,
}

#[derive(Debug)]
pub struct ParameterNode {
    pub name: String,
    pub data_type: TypeNode,
}

#[derive(Debug, PartialEq, Clone)]
pub enum TypeNode {
    IntType,
    FloatType,
    // Expand this later
}

#[derive(Debug)]
pub enum StatementNode {
    Assignment(AssignmentNode),
    // Add other statement types here
    NeedsToBeImplemented(String),
}

#[derive(Debug)]
struct AssignmentNode {
    pub variable: String,
    pub expressions: Vec<Expression>, // You can replace this with your actual expression node
}
#[derive(Debug)]
struct Expression {
    token: lexer::Token,
    precedence: Precedence,
}
// Define an enum to represent operator precedence
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Precedence {
    Lowest,
    Assignment, // Lowest precedence
    Conditional,
    Sum,     // Addition and subtraction
    Product, // Multiplication and division
    Prefix,  // Unary prefix operators
    Postfix, // Unary postfix operators
    Call,    // Function/method call
    Index,   // Array/slice indexing
    Braces,  // Parentheses (highest precedence)
    Highest, // Highest precedence
}

fn skip_tokens_while<F>(index: &mut usize, tokens: &[lexer::Token], condition: F)
where
    F: Fn(&lexer::Token) -> bool,
{
    while condition(&tokens[*index]) {
        assert!(
            tokens.len() > *index,
            "Index > Tokens! At Tokens[{}]: {:?}",
            *index,
            tokens[*index]
        );
        *index += 1;
    }
}
pub fn create_struct_ast(index: &mut usize, tokens: &[lexer::Token]) -> StructNode {
    let mut struct_name = String::new();

    if let lexer::Token::Identifier(name) = &tokens[*index + 1] {
        struct_name = name.clone();

        skip_tokens_while(index, tokens, |token| matches!(token, lexer::Token::Struct));
        skip_tokens_while(index, tokens, |token| {
            matches!(token, lexer::Token::LeftBrace)
        });
    }

    let mut fields = Vec::new();
    while let lexer::Token::Identifier(field_name) = &tokens[*index] {
        skip_tokens_while(index, tokens, |token| {
            matches!(token, lexer::Token::Identifier(_))
        });

        if let lexer::Token::Colon = tokens[*index] {
            skip_tokens_while(index, tokens, |token| matches!(token, lexer::Token::Colon));
            let field_data_type = create_type_ast(index, tokens);
            fields.push(FieldNode {
                name: field_name.clone(),
                data_type: field_data_type,
            });
        }

        skip_tokens_while(index, tokens, |token| matches!(token, lexer::Token::Comma));
    }

    skip_tokens_while(index, tokens, |token| {
        matches!(token, lexer::Token::RightBrace)
    });

    StructNode {
        name: struct_name,
        fields,
    }
}

pub fn create_function_ast(index: &mut usize, tokens: &[lexer::Token]) -> FunctionNode {
    let mut function_name = String::new();
    if let lexer::Token::Identifier(name) = &tokens[*index + 1] {
        function_name = name.clone();
        skip_tokens_while(index, tokens, |token| {
            matches!(token, lexer::Token::Identifier(_) | lexer::Token::Fun)
        });
    }

    let mut parameters = Vec::new();

    if let lexer::Token::LeftParen = tokens[*index] {
        skip_tokens_while(index, tokens, |token| {
            matches!(token, lexer::Token::LeftParen)
        });
        while let lexer::Token::Identifier(param_name) = &tokens[*index] {
            skip_tokens_while(index, tokens, |token| {
                matches!(token, lexer::Token::Identifier(_))
            });
            if let lexer::Token::Colon = tokens[*index] {
                skip_tokens_while(index, tokens, |token| {
                    matches!(token, lexer::Token::Colon | lexer::Token::Comma)
                });
                let param_data_type = create_type_ast(index, tokens);
                parameters.push(ParameterNode {
                    name: param_name.clone(),
                    data_type: param_data_type,
                });
            }

            skip_tokens_while(index, tokens, |token| matches!(token, lexer::Token::Comma));
        }
    }

    skip_tokens_while(index, tokens, |token| {
        matches!(token, lexer::Token::RightParen)
    });

    // I am just setting a default return type here. TODO: Change that Later
    let mut return_type = TypeNode::IntType;
    while let lexer::Token::Colon = tokens[*index] {
        skip_tokens_while(index, tokens, |token| matches!(token, lexer::Token::Colon));
        return_type = create_type_ast(index, tokens);
    }

    // Function starts here
    skip_tokens_while(index, tokens, |token| {
        matches!(token, lexer::Token::LeftBrace)
    });
    let mut body = Vec::new();

    // Parse statements within the function body until IdentifierClosed is encountered
    while !matches!(tokens[*index], lexer::Token::IdentifierClosed) {
        let statement = create_statement_ast(index, tokens);
        body.push(statement);
    }

    FunctionNode {
        name: function_name,
        parameters,
        return_type,
        body,
    }
}

pub fn create_statement_ast(index: &mut usize, tokens: &[lexer::Token]) -> StatementNode {
    // println!("index: {} {:?}", index, tokens[*index]);
    match &tokens[*index] {
        lexer::Token::Identifier(identifier) => {
            skip_tokens_while(index, tokens, |token| {
                matches!(token, lexer::Token::Identifier(_))
            });
            skip_tokens_while(index, tokens, |token| matches!(token, lexer::Token::Equals));

            let expressions = parse_expressions(index, tokens);

            // Skip the Semicolon token
            skip_tokens_while(index, tokens, |token| {
                matches!(token, lexer::Token::Semicolon)
            });

            return StatementNode::Assignment(AssignmentNode {
                variable: identifier.clone(),
                expressions,
            });
            // TODO Change that Later
        }
        lexer::Token::Var | lexer::Token::Const => {
            // Handle Var and Const statements here

            // For now, let's just skip these tokens
            skip_tokens_while(index, tokens, |token| {
                matches!(token, lexer::Token::Var | lexer::Token::Const)
            });
            return StatementNode::NeedsToBeImplemented(format!(
                "Var and Const statements at Lexer Index {}",
                index
            ));
        }
        lexer::Token::Return => {
            // Handle Return statements here

            // For now, let's just skip these tokens
            skip_tokens_while(index, tokens, |token| matches!(token, lexer::Token::Return));
            return StatementNode::NeedsToBeImplemented(format!(
                "Return statements at Lexer Index {}",
                index
            ));
        }
        _ => {
            // Handle other types of statements here

            // Default case: return an empty statement
            return StatementNode::NeedsToBeImplemented(format!(
                "{:?} statements at Lexer Index {}",
                tokens[*index], index
            ));
        }
    }
}

// fn parse_expression(index: &mut usize, tokens: &[lexer::Token]) -> Vec<lexer::Token> {
//     let mut expression = Vec::new();
//     while !matches!(tokens[*index], lexer::Token::Semicolon) {
//         println!("tokens[{}]: {:?}", *index, tokens[*index]);
//         expression.push(tokens[*index].clone());
//         // Skip the current token and move to the next one
//         *index += 1;
//     }
//     return expression;
// }

fn get_current_expression(index: &mut usize, tokens: &[lexer::Token]) -> Vec<Expression> {
    let mut expressions = Vec::new();
    while !matches!(tokens[*index], lexer::Token::Semicolon) {
        // get the precision of the Operator Tokens and attach it to the token we hold in Expression
        let token = tokens[*index].clone();
        let expression = Expression {
            token: token.clone(),
            precedence: precedence_of_operator(&token),
        };
        // Push the current token into the expression
        expressions.push(expression);

        // Move to the next token
        *index += 1;
    }
    return expressions;
}

fn parse_expressions(index: &mut usize, tokens: &[lexer::Token]) -> Vec<Expression> {
    // We would need to take precedence of operators into account so we can parse the expression correctly
    return get_current_expression(index, tokens);
}


fn precedence_of_operator(operator: &lexer::Token) -> Precedence {
    match operator {
        lexer::Token::Plus | lexer::Token::Minus => Precedence::Sum,
        lexer::Token::Asterisk | lexer::Token::Slash => Precedence::Product,
        lexer::Token::LeftParen => Precedence::Braces,
        _ => Precedence::Lowest, // Default precedence for other tokens
    }
}

// Create a function AST node based on lexer tokens
pub fn create_main_ast(index: &mut usize, tokens: &[lexer::Token]) -> MainFunctionNode {
    let function_node = create_function_ast(index, tokens);
    MainFunctionNode {
        function: function_node,
    }
}

pub fn create_type_ast(index: &mut usize, tokens: &[lexer::Token]) -> TypeNode {
    match &tokens[*index] {
        lexer::Token::IntType => {
            skip_tokens_while(index, tokens, |token| {
                matches!(token, lexer::Token::IntType)
            });
            return TypeNode::IntType;
        }
        lexer::Token::FloatType => {
            skip_tokens_while(index, tokens, |token| {
                matches!(token, lexer::Token::FloatType)
            });
            return TypeNode::FloatType;
        }
        // Handle other types here
        _ => panic!("Unexpected token while creating type AST"),
    }
}

pub fn create_ast(tokens: &[lexer::Token]) -> Vec<AstNode> {
    let mut ast = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        match tokens[index] {
            lexer::Token::Struct => {
                // Create StructNode and append to ast
                let struct_node = create_struct_ast(&mut index, &tokens);
                ast.push(AstNode::Struct(struct_node));
            }
            lexer::Token::Fun => {
                // Check if Fun Identifier is followed by Main
                if let lexer::Token::Identifier(name) = &tokens[index + 1] {
                    if name == "main" {
                        // Create MainNode and append to ast
                        let main_node = create_main_ast(&mut index, &tokens);
                        ast.push(AstNode::MainFunction(main_node));
                    } else {
                        // Create FunctionNode and append to ast
                        let function_node = create_function_ast(&mut index, &tokens);
                        ast.push(AstNode::Function(function_node));
                    }
                }
            }

            _ => println!("Not implemented {:?}", tokens[index]),
        }
        index += 1;
    }
    return ast;
}

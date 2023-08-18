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
    pub body: Vec<AstNode>,
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
    // Add other type nodes as needed
}

// Create a struct AST node based on lexer tokens
pub fn create_struct_ast(index: &mut usize, tokens: &[lexer::Token]) -> StructNode {
    let mut struct_name = String::new();
    if let lexer::Token::Identifier(name) = &tokens[*index + 1] {
        struct_name = name.clone();
        *index += 3; // Skip "struct", identifier, and "{"
    }

    let mut fields = Vec::new();
    while let lexer::Token::Identifier(field_name) = &tokens[*index] {
        *index += 1; // Skip field identifier
        if let lexer::Token::Colon = tokens[*index] {
            *index += 1; // Skip colon
            let field_data_type = create_type_ast(index, tokens);
            fields.push(FieldNode {
                name: field_name.clone(),
                data_type: field_data_type,
            });
        }
        if let lexer::Token::Comma = tokens[*index] {
            *index += 1; // Skip comma
        }
    }
    *index += 2; // Skip "}"

    StructNode {
        name: struct_name,
        fields,
    }
}

// Create a function AST node based on lexer tokens
pub fn create_function_ast(index: &mut usize, tokens: &[lexer::Token]) -> FunctionNode {
    let mut function_name = String::new();
    if let lexer::Token::Identifier(name) = &tokens[*index + 1] {
        function_name = name.clone();
        *index += 3; // Skip "fun", identifier, and "("
    }

    let mut parameters = Vec::new();
    while let lexer::Token::Identifier(param_name) = &tokens[*index] {
        *index += 1; // Skip parameter identifier
        if let lexer::Token::Colon = tokens[*index] {
            *index += 1; // Skip colon
            let param_data_type = create_type_ast(index, tokens);
            parameters.push(ParameterNode {
                name: param_name.clone(),
                data_type: param_data_type,
            });
        }
        if let lexer::Token::Comma = tokens[*index] {
            *index += 1; // Skip comma
        }
    }
    *index += 3; // Skip "):", return type, and "{"

    // Create the function body AST nodes
    let mut body = Vec::new();
    while let lexer::Token::IdentifierClosed = tokens[*index] {
        // TODO: Populate body with AST nodes for function body
        *index += 1;
    }

    FunctionNode {
        name: function_name,
        parameters,
        return_type: TypeNode::IntType, // Placeholder return type
        body,
    }
}

// Create a function AST node based on lexer tokens
pub fn create_main_ast(index: &mut usize, tokens: &[lexer::Token]) -> MainFunctionNode {
    let function_node = create_function_ast(index, tokens);
    MainFunctionNode{
        function: function_node
    }
}
// Create a type AST node based on lexer tokens
pub fn create_type_ast(index: &mut usize, tokens: &[lexer::Token]) -> TypeNode {
    // TODO: Implement type AST creation logic
    TypeNode::IntType // Placeholder type node
}

